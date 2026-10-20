//! The `reflection` extension: `php-src/ext/reflection` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/reflection/tests`, through `xtask phpt --subset`.
//!
//! What is here reads the linked tables through `api` and nothing else
//! (design §12): `ReflectionClass`, `ReflectionObject`,
//! `ReflectionProperty`, `ReflectionClassConstant` and
//! `ReflectionConstant` with the accessors
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
//! Rendered: `_extension_string`'s header, `Constants`, `Functions`
//! and `Classes` blocks, `_const_string`, `_class_string` with its
//! `_class_const_string`, `_property_string` and `_function_string`
//! rows over the linked tables, and `ReflectionClass::__toString` and
//! `ReflectionMethod::__toString` over the same two. What php reads off
//! one class entry after `zend_do_inheritance` and
//! `zend_do_implement_interfaces` ran is the engine's where the engine
//! keeps it: the function table is the class's linked record, its rows
//! read through `api` (`Request::method_rows`, `Request::method_row`)
//! and held as a `MethodRow`, and the interface list and the constants
//! table in their order and each method's prototype are the record's
//! (`Request::interfaces`, `Request::constants_in_order`,
//! `Request::row_prototype`). The properties with their shadows are
//! computed here by the same rules over the declarations
//! (`linked_properties`), so the words `inherits`, `overwrites`,
//! `prototype` and `ctor` are php's.
//! Reflection's own twenty-six classes are declared by `#[php_class]`
//! beside the bodies that serve them, every method the stub has, so
//! `php --re reflection` renders whole.
//! A reflector keeps php's `intern->ce` and `intern->obj` in the native
//! payload [`Reflected`], which its class's `create_object` lays down
//! (design §5.4), so the class it was asked about and the object it was
//! made from are what the renderings read.
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

mod attr;
mod class;
mod enums;
mod func;
mod lazy;
mod member;
mod signature;

use std::fmt::Write as _;
use std::ops::ControlFlow;

use php_ext::abi::{
    DependencyKind, Extension, ExtensionBuilder, FromArgument, GcTable, Instance, ObjectHandlers,
    OrFalse, PropertyPlace, Raw, Refusals, Std, This, Union, ZendExtension,
};
use php_ext::attr::php_class;
use php_ext::call::{
    Function, autoload, constant, eval_const_expr, lookup_class, lookup_function, new_object,
};
use php_ext::coerce::{Exponent, float_format};
use php_ext::decl::{
    ClassConstDecl, ClassDecl, ClassKind, ConstExpr, EnumBacking, FunctionDecl, HookKind,
    MethodDecl, Name, NameKind, Parameter, ParameterDefault, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, throw, throw_error};
use php_ext::request::{ExtensionId, MethodRow, Request};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{
    ClassId, Emalloc, Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef,
};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {
    /// `REFLECTION_G(key)`, the request's secret `getId` hashes a
    /// reference's identity under so no address is shown.
    reference_key: std::hash::RandomState,
}

/// The extension value (design §7.1): the twenty-six classes in the
/// order `PHP_MINIT_FUNCTION(reflection)` registers them, which is
/// `EG(class_table)`'s and what `php --re` walks; the stub writes
/// `PropertyHookType` earlier and registers it last.
///
/// ```php
/// var_dump(extension_loaded('reflection'));
/// ```
/// ```expect
/// bool(true)
/// ```
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
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .class::<ReflectionException>()
        .class::<member::Reflection>()
        .class::<Reflector>()
        .class::<func::ReflectionFunctionAbstract>()
        .class::<func::ReflectionFunction>()
        .class::<func::ReflectionGenerator>()
        .class::<signature::ReflectionParameter>()
        .class::<signature::ReflectionType>()
        .class::<signature::ReflectionNamedType>()
        .class::<signature::ReflectionUnionType>()
        .class::<signature::ReflectionIntersectionType>()
        .class::<func::ReflectionMethod>()
        .class::<class::ReflectionClass>()
        .class::<class::ReflectionObject>()
        .class::<member::ReflectionProperty>()
        .class::<member::ReflectionClassConstant>()
        .class::<ReflectionExtension>()
        .class::<ReflectionZendExtension>()
        .class::<ReflectionReference>()
        .class::<attr::ReflectionAttribute>()
        .class::<enums::ReflectionEnum>()
        .class::<enums::ReflectionEnumUnitCase>()
        .class::<enums::ReflectionEnumBackedCase>()
        .class::<func::ReflectionFiber>()
        .class::<member::ReflectionConstant>()
        .class::<member::PropertyHookType>()
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

/// php's `reflection_object` beside the `name` and `class` properties
/// the stub declares (`Z/ext/reflection/php_reflection.c:180-188`):
/// `intern->ce`, the class the reflector was asked about, which is not
/// always the one that declared what it reflects; and `intern->obj`,
/// the object a `ReflectionObject` was made from. `_function_string`
/// takes `intern->ce` as a method's scope, `_class_string` prints the
/// `Dynamic properties` of `intern->obj`, and
/// `ReflectionProperty::getValue` holds its argument to `intern->ce`.
/// The slot is laid down by `new` as the class's `create_object`
/// (design §5.4) and written by the constructor.
#[derive(Default)]
pub(crate) struct ReflectedState {
    ce: Option<ClassId>,
    /// `intern->obj`, an owner of the object reflected.
    object: Option<Owner<PhpObject>>,
    /// php's `attribute_reference`, which `ReflectionAttribute` keeps
    /// where the other reflectors keep `intern->ce`.
    attribute: Option<attr::AttributeRef>,
    /// php's `parameter_reference`, the parameter a
    /// `ReflectionParameter` reflects, whose subject is no name it can
    /// look up again.
    parameter: Option<signature::ParameterRef>,
    /// php's `type_reference`, the type a type reflector reflects.
    ty: Option<signature::TypeRef>,
    /// The struct `intern->ptr` points at where php's constructor or
    /// factory `emalloc`s one beside the `reflection_object`
    /// ([`Reference`]), held on the request's heap at its size and given
    /// back in `free_obj`, `reflection_free_objects_storage`.
    ptr: Option<Emalloc>,
    /// The reference a `ReflectionReference` holds in `intern->obj`, an
    /// owner of it.
    reference: Option<Owner<PhpRef>>,
    /// Whether a `ReflectionMethod` reflects a property's hook, which
    /// php's `intern->ptr` holds as the `zend_function` itself and no
    /// method table lists: the reflector's `$name`, `$prop::get` or
    /// `$prop::set`, names it among the hooks of `$class`'s property.
    hook: bool,
    /// php's `intern->ptr` of a `ReflectionMethod`: the method its
    /// constructor or factory found, which each accessor reads.
    method: Option<HeldMethod>,
}

/// The function a `ReflectionMethod` holds as php's `intern->ptr`.
///
/// A reflector over a closure's `__invoke` holds no owner of the closure,
/// so the closure is freed while the reflector stands, and its parameters
/// still answer.
///
/// ```php
/// class D { function __destruct() { echo "dtor\n"; } function m() { return fn (int $n) => $n; } }
/// $m = new ReflectionMethod((new D)->m(), '__invoke');
/// echo "after\n";
/// $p = new ReflectionParameter([(new D)->m(), '__invoke'], 0);
/// echo "after2\n";
/// var_dump((string) $m->getParameters()[0]->getType(), $p->getName(), $p->getDeclaringFunction()->getNumberOfParameters());
/// ```
/// ```expect
/// dtor
/// after
/// dtor
/// after2
/// string(3) "int"
/// string(1) "n"
/// int(1)
/// ```
#[derive(Clone, Copy)]
pub(crate) enum HeldMethod {
    /// A row of the engine's function table.
    Row(MethodRow),
    /// The `__invoke` `zend_get_closure_invoke_method` makes for a
    /// closure, `ZEND_ACC_CALL_VIA_HANDLER`, over the closure's function:
    /// the trampoline borrows the `arg_info` of the op array, which the
    /// unit holds for the request, so the reflector holds no owner of
    /// the closure and its parameters answer once the closure is freed.
    /// `None` for the closure `object_init_ex(zend_ce_closure)` makes,
    /// which holds no function.
    Invoke(Option<Function>),
}

/// The payload: the reflector's fields, boxed, inside the `sizeof` of
/// `reflection_object` php allocates (design §5.13).
pub(crate) type Reflected = Box<ReflectedState>;

/// `reflection_object_handlers`
/// (`Z/ext/reflection/php_reflection.c:7976-7981`): `free_obj` gives
/// back the object the reflector kept, `clone_obj` is php's null slot,
/// which is why `clone $reflector` is php's uncloneable `Error`, and
/// `get_gc` hands the collector what the payload holds, and
/// `write_property` refuses a write to the `$name` and `$class` the class
/// declares. php's `offset` is its own bookkeeping.
pub(crate) struct ReflectorHandlers;

/// The handlers every reflector with a [`Reflected`] payload carries.
pub(crate) static HANDLERS: ReflectorHandlers = ReflectorHandlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for ReflectorHandlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(reflected) = object.native_mut::<Reflected>(req) else {
            return;
        };
        let (kept, reference, ptr) = (
            reflected.object.take(),
            reflected.reference.take(),
            reflected.ptr.take(),
        );
        if let Some(ptr) = ptr {
            ptr.free(req);
        }
        if let Some(kept) = kept {
            kept.give_up(req);
        }
        if let Some(reference) = reference {
            reference.give_up(req);
        }
    }

    /// `_reflection_write_property`: `$name` and `$class` where the
    /// object's class declares them are php's `ReflectionException`, and
    /// every other write is `zend_std_write_property`'s.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class M extends ReflectionMethod { public $foo = 'x'; }
    /// $r = new M('M', 'getName');
    /// foreach (['class', 'name', 'foo', 'bar'] as $p) {
    ///     try { $r->$p = 'set'; echo $p, ': ', $r->$p, "\n"; } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// $c = new ReflectionClass('stdClass');
    /// try { $c->name = 'x'; } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { $c->class = 'x'; } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot set read-only property M::$class
    /// Cannot set read-only property M::$name
    /// foo: set
    /// bar: set
    /// Cannot set read-only property ReflectionClass::$name
    ///
    /// Deprecated: Creation of dynamic property ReflectionClass::$class is deprecated in %s on line 10
    /// ```
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let held = name.as_bytes().to_vec();
        if read_only(req, object, &held) {
            value.give_up(req);
            name.give_up(req);
            let class = class_name_of(req, object.class());
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Cannot set read-only property {}::${}",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&held)
                ),
            ));
        }
        Std.write_property(req, object, name, value)
    }

    /// php's `NULL` for the two names [`Self::write_property`] refuses,
    /// so a write to either reaches it.
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        if read_only(req, object, name) {
            Ok(PropertyPlace::Handlers)
        } else {
            Std.get_property_ptr_ptr(req, object, name)
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(reflected) = object.native::<Reflected>(req) else {
            return GcTable::Properties;
        };
        if let Some(kept) = &reflected.object {
            visit(kept.as_value());
        }
        if let Some(reference) = &reflected.reference {
            visit(reference.as_value());
        }
        GcTable::Properties
    }
}

/// `$name` and `$class` where the object's class declares them, the
/// two properties `_reflection_write_property` keeps a program from
/// writing.
fn read_only(req: &mut Request, object: PhpObject, name: &[u8]) -> bool {
    (name == b"name" || name == b"class") && property_named(req, object.class(), name).is_some()
}

/// `intern->ce` and `intern->obj` written as php's constructors write
/// them, the object one more owner where there is one: what `new` laid
/// down is overwritten, so nothing the payload held before is lost.
fn keep(req: &mut Request, this: PhpObject, ce: ClassId, object: Option<PhpObject>) {
    let object = object.map(|object| Owner::take(req, object));
    let released = match this.native_mut::<Reflected>(req) {
        Some(reflected) => {
            reflected.ce = Some(ce);
            std::mem::replace(&mut reflected.object, object)
        }
        None => object,
    };
    if let Some(released) = released {
        released.give_up(req);
    }
}

/// `intern->obj` alone, as `ReflectionFunction::__construct` writes it
/// for a closure (`php_reflection.c`'s `is_closure` arm): the object
/// one more owner, the class left as it stands, since a closure names
/// none.
fn keep_closure(req: &mut Request, this: PhpObject, object: PhpObject) {
    let object = Owner::take(req, object);
    let released = match this.native_mut::<Reflected>(req) {
        Some(reflected) => reflected.object.replace(object),
        None => Some(object),
    };
    if let Some(released) = released {
        released.give_up(req);
    }
}

/// The structs php's reflectors `emalloc` for `intern->ptr`, by
/// `ref_type`, each at the size the pinned build's `sizeof` gives
/// (`Z/ext/reflection/php_reflection.c:132-160`).
#[derive(Clone, Copy)]
pub(crate) enum Reference {
    /// `property_reference`: `prop`, `unmangled_name` and
    /// `cache_slot[3]`.
    Property,
    /// `parameter_reference`: `offset`, `required`, `arg_info` and
    /// `fptr`.
    Parameter,
    /// `type_reference`: the `zend_type` and `legacy_behavior`.
    Type,
    /// `attribute_reference`: `attributes`, `data`, `scope`,
    /// `filename` and `target`.
    Attribute,
}

impl Reference {
    /// `sizeof` the struct on the pinned 64-bit build.
    const fn size(self) -> usize {
        match self {
            Reference::Property | Reference::Attribute => 40,
            Reference::Parameter | Reference::Type => 24,
        }
    }
}

/// `intern->ptr = emalloc(sizeof(...))`: the struct of `kind` laid down
/// on the request's heap beside the reflector, the one it held before
/// given back first, as php's constructors free it before they
/// allocate. The `Err` is the memory limit.
fn keep_reference(req: &mut Request, this: PhpObject, kind: Reference) -> Result<(), Thrown> {
    let block = Emalloc::new(req, kind.size())?;
    let released = match this.native_mut::<Reflected>(req) {
        Some(reflected) => reflected.ptr.replace(block),
        None => Some(block),
    };
    if let Some(released) = released {
        released.free(req);
    }
    Ok(())
}

/// `intern->ptr` as `reflection_method_factory` writes it over a hook
/// (`ReflectionProperty::getHook`), which the method accessors then
/// find among the property's hooks rather than among its methods.
fn keep_hook(req: &mut Request, this: PhpObject) {
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.hook = true;
    }
}

/// Whether a `ReflectionMethod` reflects a hook ([`keep_hook`]).
fn kept_hook(req: &Request, this: PhpObject) -> bool {
    this.native::<Reflected>(req).is_some_and(|kept| kept.hook)
}

/// `intern->ptr` as `reflection_attribute_factory` writes it: the
/// reference the six `ReflectionAttribute` accessors read, in its
/// `attribute_reference`.
fn keep_attribute(
    req: &mut Request,
    this: PhpObject,
    kept: attr::AttributeRef,
) -> Result<(), Thrown> {
    keep_reference(req, this, Reference::Attribute)?;
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.attribute = Some(kept);
    }
    Ok(())
}

/// `intern->ptr` as `reflection_parameter_factory` and
/// `ReflectionParameter::__construct` write it, in its
/// `parameter_reference`.
fn keep_parameter(
    req: &mut Request,
    this: PhpObject,
    kept: signature::ParameterRef,
) -> Result<(), Thrown> {
    keep_reference(req, this, Reference::Parameter)?;
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.parameter = Some(kept);
    }
    Ok(())
}

/// The `parameter_reference` a `ReflectionParameter` keeps, none before
/// anything wrote one.
fn kept_parameter(req: &Request, this: PhpObject) -> Option<signature::ParameterRef> {
    this.native::<Reflected>(req)?.parameter.clone()
}

/// `intern->ptr` as `reflection_type_factory` writes it, in its
/// `type_reference`.
fn keep_type(req: &mut Request, this: PhpObject, kept: signature::TypeRef) -> Result<(), Thrown> {
    keep_reference(req, this, Reference::Type)?;
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.ty = Some(kept);
    }
    Ok(())
}

/// The `type_reference` a type reflector keeps, none before anything
/// wrote one.
fn kept_type(req: &Request, this: PhpObject) -> Option<signature::TypeRef> {
    this.native::<Reflected>(req)?.ty.clone()
}

/// `GET_REFLECTION_OBJECT_PTR(attr)`: the reference a
/// `ReflectionAttribute` keeps, or php's `Error` for a reflector read
/// before anything wrote one.
fn this_attribute(req: &mut Request, this: PhpObject) -> Result<attr::AttributeRef, Thrown> {
    let kept = this
        .native::<Reflected>(req)
        .and_then(|reflected| reflected.attribute.clone());
    match kept {
        Some(kept) => Ok(kept),
        None => Err(throw_error(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// `intern->obj` where it holds a `Closure`, which is what a reflector
/// over a closure keeps: none for every other reflector.
fn kept_closure(req: &Request, this: PhpObject) -> Option<PhpObject> {
    let kept = kept_object(req, this)?;
    kept.closure(req).map(|_| kept)
}

/// `intern->ptr` as `reflection_method_factory` and
/// `ReflectionMethod::__construct` write it: the method found, held, or
/// none where nothing was found ([`MethodRow`]).
fn keep_method(req: &mut Request, this: PhpObject, method: Option<HeldMethod>) {
    if let Some(reflected) = this.native_mut::<Reflected>(req) {
        reflected.method = method;
    }
}

/// The method a `ReflectionMethod` holds ([`keep_method`]); none for one
/// whose constructor never ran, and for a hook's.
fn kept_method(req: &Request, this: PhpObject) -> Option<HeldMethod> {
    this.native::<Reflected>(req)?.method
}

/// `intern->ce`, the class the reflector was asked about; `None` for a
/// reflector whose class carries no payload yet.
fn kept_class(req: &Request, this: PhpObject) -> Option<ClassId> {
    this.native::<Reflected>(req).and_then(|kept| kept.ce)
}

/// `intern->obj`, the object a `ReflectionObject` was made from.
fn kept_object(req: &Request, this: PhpObject) -> Option<PhpObject> {
    this.native::<Reflected>(req)
        .and_then(|kept| kept.object.as_deref().copied())
}

/// Core's classes and interfaces the reflectors extend, implement and
/// hand out, named for `extends`, `implements` and the types that hold
/// them.
#[php_class(extern)]
pub(crate) struct Exception;

#[php_class(extern)]
pub(crate) struct Stringable;

#[php_class(extern)]
pub(crate) struct UnitEnum;

#[php_class(extern)]
pub(crate) struct Closure;

#[php_class(extern)]
pub(crate) struct Generator;

#[php_class(extern)]
pub(crate) struct Fiber;

/// A body's value answered under its method's declared type, as php
/// hands back what the native wrote into `return_value`.
fn answer<T: FromArgument>(
    value: Result<Value, Thrown>,
    req: &mut Request,
) -> Result<Raw<T>, Thrown> {
    value.and_then(|value| Raw::from_value(req, value))
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
/// N argument(s), M given`, `zend_wrong_parameters_count_error`'s
/// singular for one.
///
/// ```php
/// class C { public $p = 1; }
/// try { (new ReflectionProperty('C', 'p'))->getValue(new C, 2); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ReflectionProperty::getValue() expects at most 1 argument, 2 given
/// ```
fn at_most_arguments(req: &mut Request, most: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count <= most {
        return Ok(());
    }
    let plural = if most == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("at most {most} argument{plural}, {count} given"),
    ))
}

/// `Z_PARAM_OBJ_OR_STR`'s value and the lookup both reflection
/// constructors do with it: an object is its class, kept beside it; a
/// string is `zend_lookup_class`, a leading `\` dropped, or php's
/// `ReflectionException` `Class "X" does not exist` with the name as
/// given.
fn class_of(
    req: &mut Request,
    given: Union<PhpObject, PhpString>,
) -> Result<(ClassId, Option<PhpObject>), Thrown> {
    match given {
        Union::A(object) => Ok((object.class(), Some(object))),
        Union::B(name) => Ok((class_named(req, name.as_bytes())?, None)),
    }
}

/// `zend_lookup_class` or the `ReflectionException` every reflector
/// throws for a miss, `Class "X" does not exist`, the name as given.
fn class_named(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    match autoload(req, name)? {
        Some(class) => Ok(class),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!("Class \"{}\" does not exist", String::from_utf8_lossy(name)),
        )),
    }
}

/// The receiver's `name` property, or php's `Error` for a reflector
/// read before its constructor ran.
fn this_name(req: &mut Request, this: PhpObject, class: &str) -> Result<Vec<u8>, Thrown> {
    match this
        .prop(req, b"name")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    {
        Some(name) => Ok(name),
        None => Err(throw_error(
            req,
            "Error",
            &format!("Typed property {class}::$name must not be accessed before initialization"),
        )),
    }
}

fn set_name(req: &mut Request, this: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    set_string_prop(req, this, b"name", name)
}

/// A reflector's own property written as php's constructors write
/// `reflection_prop_name` and `reflection_prop_class`, into the slot
/// past [`ReflectorHandlers::write_property`], which refuses a program
/// the same write. The value is a name a declaration made, which php
/// copies from `ce->name`, `fptr->common.function_name` or
/// `prop_info->name`, so it is the interned string and no allocation of
/// the reflector's, and `getName()` answers the same string:
///
/// ```php
/// $r = new ReflectionClass('stdClass'); $f = new ReflectionFunction('strlen'); $x = new ReflectionMethod('ArrayObject', 'count');
/// $m = memory_get_usage(); $s = new ReflectionClass('stdClass'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $g = new ReflectionFunction('strlen'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $y = new ReflectionMethod('ArrayObject', 'count'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $n = [$s->getName(), $g->getName(), $y->getName(), $y->class]; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 96
/// 96
/// 112
/// 216
/// ```
fn set_string_prop(
    req: &mut Request,
    this: PhpObject,
    prop: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let key = PhpString::declared(req, prop)?;
    let value = match PhpString::declared(req, value) {
        Ok(value) => value.into_value(),
        Err(thrown) => {
            key.give_up(req);
            return Err(thrown);
        }
    };
    Std.write_property(req, this, key, value)
}

/// A string of the request's as a native's return.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `GET_REFLECTION_OBJECT_PTR(module)` for a `ReflectionExtension`: the
/// extension `$name` names, looked up again as the constructor found
/// it; a name the bundle has not is the constructor's
/// `ReflectionException`.
fn this_extension(req: &mut Request, this: PhpObject) -> Result<ExtensionId, Thrown> {
    let name = this_name(req, this, "ReflectionExtension")?;
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

/// `add_extension_class`'s test over the linked table: each class the
/// extension declared, with its name, in table order.
fn extension_classes(req: &Request, extension: ExtensionId) -> Vec<(ClassId, Vec<u8>)> {
    req.declared_classes()
        .into_iter()
        .filter(|&class| req.class_extension(class) == Some(extension))
        .map(|class| (class, class_name_of(req, class)))
        // A name that starts with `\0` is a class php keeps out of
        // `EG(class_table)`, the iterator wrapper, as
        // `get_declared_classes()` skips it.
        .filter(|(_, name)| name.first() != Some(&0))
        .collect()
}

/// `zend_reflection_class_factory`: a `ReflectionClass`, or a
/// `ReflectionEnum` for an enum, with `$name` written and no
/// constructor served.
fn reflection_class_factory(req: &mut Request, class: ClassId) -> Result<Owner<PhpObject>, Thrown> {
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
    set_name(req, *object, &name)?;
    Ok(object)
}

/// `_extension_ini_string` over each of the extension's entries: the
/// name, `<ALL>` or the `USER,PERDIR,SYSTEM` bits it may be changed
/// at, the value in force and, where the request changed it, the value
/// it had before.
///
/// ```php
/// ini_set('date.default_latitude', '1');
/// $s = (string) new ReflectionExtension('date');
/// echo substr($s, 0, strpos($s, '  - Constants'));
/// ```
/// ```expect
/// Extension [ <persistent> extension #1 date version %s ] {
///
///   - INI {
///     Entry [ date.timezone <ALL> ]
///       Current = 'UTC'
///     }
///     Entry [ date.default_latitude <ALL> ]
///       Current = '1'
///       Default = '31.7667'
///     }
///     Entry [ date.default_longitude <ALL> ]
///       Current = '35.2333'
///     }
///     Entry [ date.sunset_zenith <ALL> ]
///       Current = '90.833333'
///     }
///     Entry [ date.sunrise_zenith <ALL> ]
///       Current = '90.833333'
///     }
///   }
/// ```
fn extension_ini_string(req: &Request, extension: ExtensionId, indent: &str) -> String {
    let mut out = String::new();
    for entry in req.extension_ini(extension) {
        let decl = &entry.decl;
        let modifiable = if decl.user && decl.per_dir && decl.system {
            String::from("ALL")
        } else {
            [
                (decl.user, "USER"),
                (decl.per_dir, "PERDIR"),
                (decl.system, "SYSTEM"),
            ]
            .iter()
            .filter(|(set, _)| *set)
            .map(|(_, word)| *word)
            .collect::<Vec<_>>()
            .join(",")
        };
        writeln!(
            out,
            "    {indent}Entry [ {} <{modifiable}> ]",
            String::from_utf8_lossy(&decl.name.0)
        )
        .ok();
        writeln!(
            out,
            "    {indent}  Current = '{}'",
            String::from_utf8_lossy(entry.value.as_deref().unwrap_or_default())
        )
        .ok();
        if entry.modified {
            writeln!(
                out,
                "    {indent}  Default = '{}'",
                String::from_utf8_lossy(entry.master.as_deref().unwrap_or_default())
            )
            .ok();
        }
        writeln!(out, "    {indent}}}").ok();
    }
    out
}

// -------------------------------------------------- ReflectionReference

// -------------------------------------------------- ReflectionZendExtension

// -------------------------------------------------- ReflectionClass

/// `GET_REFLECTION_OBJECT_PTR(ce)` for a class reflector: the linked
/// class `$name` names on `ZEND_THIS`, looked up again since the
/// reflector keeps the name and not php's `intern->ptr`.
fn this_class(req: &mut Request, this: PhpObject) -> Result<ClassId, Thrown> {
    let name = this_name(req, this, "ReflectionClass")?;
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
    req.lineage(class).collect()
}

/// `zend_memrchr(name, '\\')`: where the namespace ends, if the name
/// has one.
fn namespace_end(name: &[u8]) -> Option<usize> {
    name.iter().rposition(|&b| b == b'\\')
}

/// The key `zend_register_constant` writes a constant under, made of
/// the name as a program wrote it: the leading `\` dropped, as
/// `zend_lookup_class` drops it, and the namespace lowered, the name
/// itself left as written.
fn constant_key(name: &[u8]) -> Vec<u8> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    let mut key = bare.to_vec();
    if let Some(end) = namespace_end(bare) {
        key[..end].make_ascii_lowercase();
    }
    key
}

fn is_internal(req: &Request, class: ClassId) -> bool {
    req.class_decl(class)
        .is_none_or(|decl| decl.file.0.is_empty())
}

fn user_line(req: &Request, class: ClassId, line: fn(&ClassDecl) -> u32) -> OrFalse<i64> {
    req.class_decl(class)
        .filter(|decl| !decl.file.0.is_empty())
        .map_or(OrFalse::False, |decl| OrFalse::Value(i64::from(line(decl))))
}

fn class_extension_name(req: &Request, class: ClassId) -> Option<String> {
    let extension = req.class_extension(class)?;
    req.extension_name(extension).map(str::to_owned)
}

/// The constant `name` names in `ce->constants_table`: the class's own,
/// a parent's non-private one or an interface's, with the class that
/// declares it.
///
/// ```php
/// $rc = new ReflectionClass('DateTime');
/// var_dump($rc->hasConstant('ATOM'), $rc->getReflectionConstant('ATOM')->class);
/// ```
/// ```expect
/// bool(true)
/// string(17) "DateTimeInterface"
/// ```
fn class_constant_named(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, ClassConstDecl)> {
    req.class_constant_of(class, name)
        .map(|(declaring, constant)| (declaring, constant.clone()))
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
/// A class constant's value as the constants table holds it once it is
/// updated (`zend_update_class_constant`): an enum's case is the enum's
/// instance for it (`zend_enum_get_case`), and every other constant the
/// expression it was written as, evaluated in the scope that declared
/// it and held to its type.
fn constant_value(
    req: &mut Request,
    constant: &php_ext::decl::ClassConstDecl,
    scope: ClassId,
) -> Result<Owner<Value>, Thrown> {
    php_ext::call::class_constant(req, scope, &constant.name.0)
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

// -------------------------------------------------- ReflectionClass::__toString

// -------------------------------------------------- ReflectionMethod

// -------------------------------------------------- ReflectionProperty

/// The property a `ReflectionProperty` reflects: its declaring class
/// (`$class`), its name, and its declaration where the class has one,
/// none for a dynamic property.
fn this_property(
    req: &mut Request,
    this: PhpObject,
) -> Result<(ClassId, Vec<u8>, Option<PropertyDecl>), Thrown> {
    let name = this_name(req, this, "ReflectionProperty")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionProperty::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let declared = property_named(req, class, &name).map(|(_, decl)| decl);
    Ok((class, name, declared))
}

/// `instanceof_function(Z_OBJCE_P(object), ce)` or php's refusal.
fn held_to(req: &mut Request, object: PhpObject, class: ClassId) -> Result<(), Thrown> {
    if object.instance_of(req, class) {
        return Ok(());
    }
    Err(throw(
        req,
        "ReflectionException",
        "Given object is not an instance of the class this property was declared in",
    ))
}

/// `intern->ce`, the class a property reflector was asked about, or
/// the declaring class where the payload has none.
fn asked_class(req: &mut Request, this: PhpObject, declaring: ClassId) -> ClassId {
    kept_class(req, this).unwrap_or(declaring)
}

/// `reflection_property_get_effective_prop`: the property the raw
/// accessors act on, the one the object's own class holds under the name
/// (its `properties_info`, a parent's private one among them) where the
/// object is not of the class the reflector was asked about and the
/// reflected property is not private, since the object's class may
/// redeclare it with hooks of its own, or none; the reflected one
/// otherwise. Each with the class that declared it; none for a dynamic
/// property.
///
/// ```php
/// #[AllowDynamicProperties] class B { public $v { get => 1; set {} } }
/// class T extends B { public $v { get => $this->v; set => $value; } public $d { get => $this->d; set => $value; } public static $s; }
/// $b = new B; $b->d = 0; $b->s = 0;
/// foreach ([[new ReflectionProperty('B', 'v'), 'v'], [new ReflectionProperty($b, 'd'), 'd'], [new ReflectionProperty($b, 's'), 's']] as [$r, $n]) {
///     $t = new T;
///     try { $r->setRawValueWithoutLazyInitialization($t, 43); var_dump($r->getRawValue($t)); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(43)
/// int(43)
/// Can not use setRawValueWithoutLazyInitialization on static property T::$s
/// ```
fn effective_prop(
    req: &mut Request,
    asked: ClassId,
    object: PhpObject,
    reflected: Option<(ClassId, PropertyDecl)>,
    name: &[u8],
) -> Option<(ClassId, PropertyDecl)> {
    let private = reflected
        .as_ref()
        .is_some_and(|(_, decl)| decl.visibility == Visibility::Private);
    if asked == object.class() || private {
        return reflected;
    }
    class_chain(req, object.class()).into_iter().find_map(|at| {
        let decl = req.class_decl(at)?;
        let found = decl.properties.iter().find(|p| &*p.name.0 == name)?;
        Some((at, found.clone()))
    })
}

// -------------------------------------------------- ReflectionClassConstant

/// The constant a `ReflectionClassConstant` reflects, looked up again
/// in the class `$class` names as php's `intern->ptr` points at the
/// entry it found: `None` for a name the table no longer has.
fn this_class_constant(
    req: &mut Request,
    this: PhpObject,
) -> Result<Option<ClassConstDecl>, Thrown> {
    let name = this_name(req, this, "ReflectionClassConstant")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw_error(
            req,
            "Error",
            "Typed property ReflectionClassConstant::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    Ok(class_constant_named(req, class, &name).map(|(_, decl)| decl))
}

// -------------------------------------------------- ReflectionConstant

/// `_extension_string`: the header with the extension's number and
/// version, then the blocks it has something for: `Dependencies`, one
/// row per `zend_module_dep`; `Constants`, one
/// `_const_string` row per constant the extension declared;
/// `Functions`, one `_function_string` per native; `Classes`, one
/// `_class_string` per class, each preceded by a blank line as
/// `_extension_class_string` writes it. Every bundled extension is
/// `<persistent>`, as one compiled in is, and its version is the
/// extension's own. The `INI` block is not rendered yet (#375); `json` has
/// no ini entry.
fn extension_string(
    req: &mut Request,
    extension: ExtensionId,
    indent: &str,
) -> Result<String, Thrown> {
    let mut out = String::new();
    let name = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    let version = req.extension_version(extension).unwrap_or_default();
    writeln!(
        out,
        "{indent}Extension [ <persistent> extension #{} {name} version {version} ] {{",
        req.extension_number(extension)
    )
    .ok();
    let dependencies = req.extension_dependencies(extension);
    if !dependencies.is_empty() {
        out.push_str("\n  - Dependencies {\n");
        for dependency in dependencies {
            let kind = match dependency.kind {
                DependencyKind::Required => "Required",
                DependencyKind::Conflicts => "Conflicts",
                DependencyKind::Optional => "Optional",
            };
            let rel = dependency.rel.map(|r| format!(" {r}")).unwrap_or_default();
            let version = dependency
                .version
                .map(|v| format!(" {v}"))
                .unwrap_or_default();
            writeln!(
                out,
                "{indent}    Dependency [ {} ({kind}{rel}{version}) ]",
                dependency.name
            )
            .ok();
        }
        writeln!(out, "{indent}  }}").ok();
    }
    let ini = extension_ini_string(req, extension, indent);
    if !ini.is_empty() {
        out.push_str("\n  - INI {\n");
        out.push_str(&ini);
        writeln!(out, "{indent}  }}").ok();
    }
    let constants = req.extension_constants(extension);
    if !constants.is_empty() {
        write!(out, "\n  - Constants [{}] {{\n", constants.len()).ok();
        for constant in &constants {
            out.push_str(&const_string(req, &constant.0, "    "));
        }
        writeln!(out, "{indent}  }}").ok();
    }
    let functions = req.extension_functions(extension);
    if !functions.is_empty() {
        out.push_str("\n  - Functions {\n");
        for function in functions {
            out.push_str(&function_string(req, function, "    "));
        }
        writeln!(out, "{indent}  }}").ok();
    }
    let classes = extension_classes(req, extension);
    if !classes.is_empty() {
        write!(out, "\n  - Classes [{}] {{", classes.len()).ok();
        let sub_indent = format!("{indent}    ");
        for (class, _) in classes {
            out.push('\n');
            out.push_str(&class_string(req, class, None, &sub_indent)?);
        }
        writeln!(out, "{indent}  }}").ok();
    }
    writeln!(out, "{indent}}}").ok();
    Ok(out)
}

/// `_const_string`: `Constant [ <persistent> type NAME ] { value }`,
/// the flags every constant an extension registers at `MINIT` carries
/// (`CONST_PERSISTENT`; `no_file_cache` and `deprecated` are none of
/// the bundle's yet), the value's `zend_zval_type_name`, and the value
/// as `zval_get_string` prints it, `Array` for an array. The value is
/// the linked table's, read as `constant()` reads it.
fn const_string(req: &mut Request, name: &[u8], indent: &str) -> String {
    let value = constant(req, name).ok().flatten().unwrap_or_default();
    let shown = value_string(req, &value);
    // `DUMP_CONST_FLAG` (`php_reflection.c`'s `_const_string`): the
    // words of the flags the constant carries, in `CONST_PERSISTENT`,
    // `CONST_NO_FILE_CACHE`, `CONST_DEPRECATED` order, and no `<>` at
    // all where it carries none. Nothing declares the middle one here.
    let mut flags = Vec::new();
    if constant_is_persistent(req, name) {
        flags.push("persistent");
    }
    if constant_is_deprecated(req, name) {
        flags.push("deprecated");
    }
    let words = if flags.is_empty() {
        String::new()
    } else {
        format!("<{}> ", flags.join(", "))
    };
    format!(
        "{indent}Constant [ {words}{} {} ] {{ {shown} }}\n",
        value.type_name(),
        String::from_utf8_lossy(name)
    )
}

/// `CONST_PERSISTENT`: the bit every constant an extension registered
/// at startup carries and one a running program defined does not,
/// which is `get_defined_constants()`'s own list here.
fn constant_is_persistent(req: &mut Request, name: &[u8]) -> bool {
    let Ok(table) = req.constants() else {
        return true;
    };
    let defined = table.find(name).is_some();
    table.give_up(req);
    !defined
}

/// `CONST_DEPRECATED`: the bit `gen_stub` raises from a `#[\Deprecated]`
/// in the stub and `zend_compile_attributes`' validator from one a
/// program wrote, so the attribute on the declaration is the whole of
/// it (`php_unit::decl::ConstDecl`).
fn constant_is_deprecated(req: &Request, name: &[u8]) -> bool {
    req.constant_attributes(&constant_key(name))
        .iter()
        .any(|one| one.name.0.eq_ignore_ascii_case(b"Deprecated"))
}

/// A constant's value as `_const_string` and `_class_const_string`
/// print it: `Array` for an array, `Object` for an object, a float
/// through `smart_str_append_double` at `precision`, which raises
/// nothing for `NAN` where a cast to string warns since 8.5, and
/// `zval_get_string` for the rest.
///
/// ```php
/// echo new ReflectionConstant('NAN'), new ReflectionConstant('M_PI'), new ReflectionConstant('PHP_FLOAT_MAX');
/// class C { const F = NAN; const G = -INF; const H = 0.1 + 0.2; const I = 1e25; }
/// echo new ReflectionClassConstant('C', 'F'), new ReflectionClassConstant('C', 'G'), new ReflectionClassConstant('C', 'H'), new ReflectionClassConstant('C', 'I');
/// ini_set('precision', 3); echo new ReflectionConstant('M_PI');
/// ```
/// ```expect
/// Constant [ <persistent> float NAN ] { NAN }
/// Constant [ <persistent> float M_PI ] { 3.1415926535898 }
/// Constant [ <persistent> float PHP_FLOAT_MAX ] { 1.7976931348623E+308 }
/// Constant [ public float F ] { NAN }
/// Constant [ public float G ] { -INF }
/// Constant [ public float H ] { 0.3 }
/// Constant [ public float I ] { 1.0E+25 }
/// Constant [ <persistent> float M_PI ] { 3.14 }
/// ```
fn value_string(req: &mut Request, value: &Value) -> String {
    match value.deref().as_ref() {
        ValueRef::Array(_) => String::from("Array"),
        ValueRef::Object(_) => String::from("Object"),
        ValueRef::Float(number) => float_format(number, req.precision(), Exponent::Upper),
        _ => php_ext::coerce::with_string(req, value, |s| String::from_utf8_lossy(s).into_owned())
            .unwrap_or_default(),
    }
}

/// `_class_string`: the header, then the `Constants`,
/// `Static properties`, `Static methods`, `Properties`,
/// `Dynamic properties` and `Methods` blocks in php's order, an enum's
/// `Enum cases` before
/// its `Constants`, each over the linked table with the counts php
/// prints, a parent's private member skipped as its shadow is
/// (`prop->ce != ce`, `mptr->common.scope != ce`). A user class carries
/// its `@@ file start-end` line. `object` is php's `obj` argument, the
/// instance an object reflector kept: it turns the header's word into
/// `Object of class` and is the only thing that puts a `Dynamic
/// properties` block there at all. A class with `ce->get_iterator`,
/// one `Traversable` through `Iterator` or `IteratorAggregate`, is
/// marked `<iterateable>`.
///
/// ```php
/// class A implements IteratorAggregate { function getIterator(): Iterator { yield 1; } }
/// echo new ReflectionClass("A");
/// ```
/// ```expect
/// Class [ <user> <iterateable> class A implements IteratorAggregate, Traversable ] {
///   @@ %s 2-2
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [1] {
///     Method [ <user, prototype IteratorAggregate> public method getIterator ] {
///       @@ %s 2 - 2
///
///       - Parameters [0] {
///       }
///       - Return [ Iterator ]
///     }
///   }
/// }
/// ```
fn class_string(
    req: &mut Request,
    class: ClassId,
    object: Option<PhpObject>,
    indent: &str,
) -> Result<String, Thrown> {
    let mut out = String::new();
    let Some(decl) = req.class_decl(class).cloned() else {
        return Ok(out);
    };
    let sub_indent = format!("{indent}    ");
    let methods = table_methods(req, class);
    class_header(
        req,
        &mut out,
        class,
        &decl,
        &methods,
        object.is_some(),
        indent,
    );

    // `ZEND_CLASS_CONST_IS_CASE`: an enum's cases go first, under a
    // heading shown only when there are any, and its own constants
    // after them.
    let (cases, constants): (Vec<_>, Vec<_>) = constant_rows(req, class)
        .into_iter()
        .partition(|(constant, _)| constant.is_case);
    if !cases.is_empty() {
        write!(out, "\n{indent}  - Enum cases [{}] {{\n", cases.len()).ok();
        for (case, _) in &cases {
            out.push_str(&enum_case_string(req, case, decl.backing, &sub_indent));
        }
        writeln!(out, "{indent}  }}").ok();
    }
    write!(out, "\n{indent}  - Constants [{}] {{\n", constants.len()).ok();
    for (constant, scope) in &constants {
        out.push_str(&class_const_string(
            req,
            constant,
            *scope,
            None,
            &sub_indent,
        )?);
    }
    writeln!(out, "{indent}  }}").ok();

    let properties: Vec<(PropertyDecl, ClassId)> = linked_properties(req, class)
        .into_iter()
        .filter(|(p, declaring)| p.visibility != Visibility::Private || *declaring == class)
        .collect();
    let visible: Vec<&LinkedMethod> = methods
        .iter()
        .filter(|m| m.decl.visibility != Visibility::Private || m.scope == class)
        .collect();
    for is_instance in [false, true] {
        let block = if is_instance {
            "Properties"
        } else {
            "Static properties"
        };
        let chosen: Vec<&(PropertyDecl, ClassId)> = properties
            .iter()
            .filter(|(p, _)| p.is_instance == is_instance)
            .collect();
        write!(out, "\n{indent}  - {block} [{}] {{\n", chosen.len()).ok();
        for (property, declaring) in chosen {
            out.push_str(&property_string(req, *declaring, property, &sub_indent));
        }
        writeln!(out, "{indent}  }}").ok();

        // `zend_get_properties_no_lazy_init(Z_OBJ_P(obj))` less every
        // name `ce->properties_info` has and every mangled one, which
        // is how php skips a private or protected slot: what is left is
        // the object's own, and the block stands only for an object
        // reflector, after `Properties` and before `Methods`.
        if is_instance && let Some(object) = object {
            let dynamic = undeclared_properties(req, class, object);
            write!(
                out,
                "\n{indent}  - Dynamic properties [{}] {{\n",
                dynamic.len()
            )
            .ok();
            for name in &dynamic {
                writeln!(
                    out,
                    "{sub_indent}Property [ <dynamic> public ${} ]",
                    String::from_utf8_lossy(name)
                )
                .ok();
            }
            writeln!(out, "{indent}  }}").ok();
        }

        let block = if is_instance {
            "Methods"
        } else {
            "Static methods"
        };
        let chosen: Vec<&&LinkedMethod> = visible
            .iter()
            .filter(|m| m.decl.is_instance == is_instance)
            .collect();
        write!(out, "\n{indent}  - {block} [{}] {{", chosen.len()).ok();
        if chosen.is_empty() {
            out.push('\n');
        }
        for method in chosen {
            out.push('\n');
            out.push_str(&method_string(req, method, class, &sub_indent));
        }
        writeln!(out, "{indent}  }}").ok();
    }
    writeln!(out, "{indent}}}").ok();
    Ok(out)
}

/// The names in the object's property table the class did not declare,
/// in the table's order: `_adddynproperty` over `get_properties`, which
/// passes over every declared slot, `IS_INDIRECT`, and keeps the rest.
/// A slot of a public property stands under its bare name and any other
/// under a mangled one, so a row of a bare name no public property of
/// the chain declares is a dynamic property, one named as a parent's
/// private property among them. `_class_string`'s count is another rule
/// ([`undeclared_properties`]).
///
/// ```php
/// class P { private $v = 1; }
/// #[AllowDynamicProperties] class K extends P {}
/// $o = new K; $o->v = 2;
/// $r = new ReflectionObject($o);
/// var_dump(count($r->getProperties()), $r->hasProperty('v'), $r->getProperty('v')->class);
/// var_dump(count((new ReflectionObject(new DateTime))->getProperties()));
/// ```
/// ```expect
/// int(1)
/// bool(true)
/// string(1) "K"
/// int(0)
/// ```
fn dynamic_properties(req: &mut Request, class: ClassId, object: PhpObject) -> Vec<Vec<u8>> {
    let mut public: Vec<Vec<u8>> = Vec::new();
    for at in class_chain(req, class) {
        let Some(decl) = req.class_decl(at) else {
            continue;
        };
        let names = decl.properties.iter().filter(|p| p.is_instance);
        public.extend(
            names
                .filter(|p| p.visibility == Visibility::Public)
                .map(|p| p.name.0.to_vec()),
        );
    }
    // `classname_obj->handlers->get_properties(classname_obj)`: the
    // table the class answers, a `DateInterval`'s fields among it.
    let handlers = object.handlers(req);
    let Ok(table) = handlers.get_properties(req, object) else {
        return Vec::new();
    };
    // An integer key is no property, as `_adddynproperty` ignores it.
    walk::rows(req, table)
        .filter_map(|(key, _)| match key {
            Key::Str(name) => Some(name.as_bytes().to_vec()),
            Key::Int(_) => None,
        })
        .filter(|name| name.first().is_some_and(|byte| *byte != 0) && !public.contains(name))
        .collect()
}

/// `_class_string`'s `Dynamic properties`: the bare names of
/// `zend_get_properties_no_lazy_init` that `ce->properties_info` has not,
/// which holds a parent's private property too, so a dynamic property
/// named as one is not counted there. A lazy object is read as it
/// stands and not initialised; any other is the rows its class's
/// `get_properties` answers.
///
/// ```php
/// class C { public int $a; }
/// $r = new ReflectionClass(C::class);
/// $g = $r->newLazyGhost(function () { echo "init\n"; });
/// $s = (string) new ReflectionObject($g);
/// var_dump($r->isUninitializedLazyObject($g));
/// preg_match("/Dynamic properties \\[(\\d+)\\]/", (string) new ReflectionObject(new DateInterval("P1D")), $m); echo $m[1], "\n";
/// ```
/// ```expect
/// bool(true)
/// 10
/// ```
fn undeclared_properties(req: &mut Request, class: ClassId, object: PhpObject) -> Vec<Vec<u8>> {
    let declared: Vec<Vec<u8>> = class_chain(req, class)
        .into_iter()
        .filter_map(|at| req.class_decl(at))
        .flat_map(|decl| decl.properties.iter().map(|p| p.name.0.to_vec()))
        .collect();
    // `zend_get_properties_no_lazy_init`: a lazy object's rows as they
    // stand, nothing initialised, and any other's class table, a
    // `DateInterval`'s fields among it.
    let entries = if object.is_lazy() {
        object.props(req)
    } else {
        let handlers = object.handlers(req);
        handlers
            .get_properties(req, object)
            .map(|table| walk::rows(req, table).collect())
    };
    let Ok(entries) = entries else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|(key, _)| match key {
            Key::Str(name) => Some(name.as_bytes().to_vec()),
            Key::Int(_) => None,
        })
        .filter(|name| name.first().is_some_and(|byte| *byte != 0))
        .filter(|name| !declared.iter().any(|shown| shown == name))
        .collect()
}

/// The first line of `_class_string`: the doc comment where there is
/// one, `Object of class` for an object reflector and the kind
/// otherwise, `<internal:ext>` or `<user>`, the modifiers, the name,
/// `extends`, an enum's backing type and the interface list, then a
/// user class's `@@ file start-end` line. `extends` names the parent as
/// it was declared, `ce->parent->name`, whatever case the child's
/// `extends` spelled it in:
///
/// ```php
/// class Par {} class C extends par {}
/// echo explode("\n", (string) new ReflectionClass('C'))[0], "\n";
/// ```
/// ```expect
/// Class [ <user> class C extends Par ] {
/// ```
fn class_header(
    req: &mut Request,
    out: &mut String,
    class: ClassId,
    decl: &ClassDecl,
    methods: &[LinkedMethod],
    of_object: bool,
    indent: &str,
) {
    if let Some(doc) = &decl.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    let kind = match (of_object, decl.kind) {
        (true, _) => "Object of class",
        (false, ClassKind::Class) => "Class",
        (false, ClassKind::Interface) => "Interface",
        (false, ClassKind::Trait) => "Trait",
        (false, ClassKind::Enum) => "Enum",
    };
    write!(out, "{indent}{kind} [ ").ok();
    let internal = decl.file.0.is_empty();
    out.push_str(if internal { "<internal" } else { "<user" });
    if internal && let Some(extension) = req.class_extension_name(class) {
        write!(out, ":{extension}").ok();
    }
    out.push_str("> ");
    // `ce->get_iterator`, which `zend_implement_iterator` and
    // `zend_implement_aggregate` give a class that is `Traversable`
    // through `Iterator` or `IteratorAggregate`, and `Generator` has,
    // and `zend_compile_prop_decl` gives a class declaring a hooked
    // property, `zend_hooked_object_get_iterator`, which a child
    // inherits.
    let traversable = matches!(decl.kind, ClassKind::Class | ClassKind::Enum)
        && lookup_class(req, b"Traversable")
            .ok()
            .flatten()
            .is_some_and(|traversable| req.instance_of(class, traversable));
    let iterable = traversable
        || req.class_own_iterator(class)
        || linked_properties(req, class)
            .iter()
            .any(|(property, _)| property.hooks.is_some());
    if iterable {
        out.push_str("<iterateable> ");
    }
    match decl.kind {
        ClassKind::Interface => out.push_str("interface "),
        ClassKind::Trait => out.push_str("trait "),
        ClassKind::Enum => out.push_str("enum "),
        ClassKind::Class => {
            // `ZEND_ACC_IMPLICIT_ABSTRACT_CLASS`: a class with an
            // abstract method it did not implement is abstract too.
            if decl.flags.is_abstract || methods.iter().any(|m| m.decl.is_abstract) {
                out.push_str("abstract ");
            }
            if decl.flags.is_final {
                out.push_str("final ");
            }
            if decl.flags.is_readonly {
                out.push_str("readonly ");
            }
            out.push_str("class ");
        }
    }
    out.push_str(&String::from_utf8_lossy(&decl.name.0));
    // `ce->parent->name`: the parent as it was declared, not as the
    // child's `extends` spelled it.
    if let Some(parent) = req.class_parent(class) {
        let name = class_name_of(req, parent);
        write!(out, " extends {}", String::from_utf8_lossy(&name)).ok();
    }
    if decl.kind == ClassKind::Enum {
        match decl.backing {
            EnumBacking::Int => out.push_str(": int"),
            EnumBacking::String => out.push_str(": string"),
            EnumBacking::Pure => {}
        }
    }
    let interfaces = req.interfaces(class).to_vec();
    for (i, interface) in interfaces.iter().enumerate() {
        let name = class_name_of(req, *interface);
        let name = String::from_utf8_lossy(&name);
        if i == 0 {
            let word = if decl.kind == ClassKind::Interface {
                "extends"
            } else {
                "implements"
            };
            write!(out, " {word} {name}").ok();
        } else {
            write!(out, ", {name}").ok();
        }
    }
    out.push_str(" ] {\n");
    if !internal {
        writeln!(
            out,
            "{indent}  @@ {} {}-{}",
            String::from_utf8_lossy(&decl.file.0),
            decl.line,
            decl.end_line
        )
        .ok();
    }
}

/// `_class_const_string`: `Constant [ final? visibility type NAME ] {
/// value }`, the type the declaration's where it has one and the
/// value's `zend_zval_type_name` otherwise, the value as
/// `zval_get_string` prints it, `Array` for an array and `Object` for
/// an object; a doc comment on the line above. The value is evaluated
/// in the class that declared it, so a private constant of that class
/// is in reach from a subclass's rendering, and a failure is the
/// `Thrown` it raised, which the whole `__toString` gives up for.
///
/// ```php
/// class B { const X = NOPE; }
/// try { echo new ReflectionClassConstant('B', 'X'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { echo new ReflectionClass('B'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class A { private const T = 't'; const L = self::T . 'x'; } class C extends A {}
/// echo new ReflectionClassConstant('C', 'L');
/// ```
/// ```expect
/// Undefined constant "NOPE"
/// Undefined constant "NOPE"
/// Constant [ public string L ] { tx }
/// ```
fn class_const_string(
    req: &mut Request,
    constant: &ClassConstDecl,
    scope: ClassId,
    enum_of: Option<&[u8]>,
    indent: &str,
) -> Result<String, Thrown> {
    let mut out = String::new();
    if let Some(doc) = &constant.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    // A case of an enum holds the enum's own instance, so
    // `zend_zval_type_name` names the enum and the `IS_OBJECT` arm
    // writes `Object` in place of the value.
    // `zend_update_class_constant(c, name, c->ce)`: evaluated in the
    // class that declared it and held to its type, a failure the
    // exception it raised and no rendering.
    let value = constant_value(req, constant, scope)?;
    let ty = match (&constant.ty, enum_of) {
        (Some(ty), _) => type_to_string(ty),
        (None, Some(name)) => String::from_utf8_lossy(name).into_owned(),
        (None, None) => String::from(value.type_name()),
    };
    let shown = match enum_of {
        Some(_) => String::from("Object"),
        None => value_string(req, &value),
    };
    value.give_up(req);
    writeln!(
        out,
        "{indent}Constant [ {}{} {ty} {} ] {{ {shown} }}",
        if constant.is_final { "final " } else { "" },
        visibility_word(constant.visibility),
        String::from_utf8_lossy(&constant.name.0)
    )
    .ok();
    Ok(out)
}

/// `_enum_case_string`: `Case NAME` for a pure enum, `Case NAME = value`
/// for a backed one, the value being the case's backing value as
/// `zval_get_string` prints it; a doc comment on the line above.
fn enum_case_string(
    req: &mut Request,
    case: &ClassConstDecl,
    backing: EnumBacking,
    indent: &str,
) -> String {
    let mut out = String::new();
    if let Some(doc) = &case.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    write!(
        out,
        "{indent}Case {}",
        String::from_utf8_lossy(&case.name.0)
    )
    .ok();
    if backing != EnumBacking::Pure {
        let value = eval_const_expr(req, &case.value, None).unwrap_or_else(|_| Owner::null());
        write!(out, " = {}", value_string(req, &value)).ok();
        value.give_up(req);
    }
    out.push('\n');
    out
}

/// `zend_visibility_string`.
fn visibility_word(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// `_property_string` for a declared property: `Property [ visibility
/// type $name = default ]` with the modifier words between where the
/// declaration has them, the default being the
/// slot of `default_properties_table` as `format_default_value` prints
/// it: an untyped property nothing initialised holds `NULL`, a typed
/// one is `UNDEF` and prints no default, and so is a virtual one. The
/// `{ get; set; }` list is `prop->hooks` as the table of `declaring`
/// holds it, the inherited ones included, each `final` where it is.
fn property_string(
    req: &mut Request,
    declaring: ClassId,
    property: &PropertyDecl,
    indent: &str,
) -> String {
    let mut out = String::new();
    if let Some(doc) = &property.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    out.push_str(indent);
    out.push_str("Property [ ");
    // `_property_string`'s order: `abstract`, `final`, the visibility,
    // the set visibility, `static`, `readonly`, `virtual`, the type.
    if property
        .hooks
        .as_ref()
        .is_some_and(|hooks| hooks.is_abstract)
    {
        out.push_str("abstract ");
    }
    if property.is_final {
        out.push_str("final ");
    }
    out.push_str(visibility_word(property.visibility));
    out.push(' ');
    // `zend_declare_typed_property`: a readonly property with no set
    // visibility of its own is `ZEND_ACC_PROTECTED_SET`, which the
    // `(set)` words print; the implicit one waits on the readonly
    // modification error that php raises before it.
    match property.set_visibility {
        Some(Visibility::Private) => out.push_str("private(set) "),
        Some(_) => out.push_str("protected(set) "),
        None if property.is_readonly && property.visibility == Visibility::Public => {
            out.push_str("protected(set) ");
        }
        None => {}
    }
    if !property.is_instance {
        out.push_str("static ");
    }
    if property.is_readonly {
        out.push_str("readonly ");
    }
    let name = &property.name.0;
    let is_virtual = php_ext::call::property_is_virtual(req, declaring, name);
    if is_virtual {
        out.push_str("virtual ");
    }
    if let Some(ty) = &property.ty {
        write!(out, "{} ", type_to_string(ty)).ok();
    }
    write!(out, "${}", String::from_utf8_lossy(name)).ok();
    let evaluated = (property.is_instance)
        .then(|| req.evaluated_default(declaring, name))
        .flatten();
    match (&property.default, &property.ty) {
        _ if is_virtual => {}
        (Some(_), _) if let Some(value) = evaluated => {
            write!(out, " = {}", value_default_string(req, &value)).ok();
        }
        (Some(default), _) => {
            write!(out, " = {}", default_value_string(req, default)).ok();
        }
        (None, None) => out.push_str(" = NULL"),
        (None, Some(_)) => {}
    }
    let hooks: Vec<(HookKind, bool)> = HookKind::ALL
        .into_iter()
        .filter_map(|kind| {
            php_ext::call::property_hook(req, declaring, name, kind)
                .map(|(_, hook)| (kind, hook.is_final))
        })
        .collect();
    // `prop->hooks`, which an internal class's virtual property with no
    // hook of its own has not, `DatePeriod`'s seven.
    let hooked = property
        .hooks
        .as_ref()
        .is_some_and(|own| own.get.is_some() || own.set.is_some());
    if hooked || !hooks.is_empty() {
        out.push_str(" {");
        for (kind, is_final) in hooks {
            write!(
                out,
                " {}{};",
                if is_final { "final " } else { "" },
                kind.name()
            )
            .ok();
        }
        out.push_str(" }");
    }
    out.push_str(" ]\n");
    out
}

/// `format_default_value` over a default already evaluated
/// ([`Request::evaluated_default`]): a scalar as `smart_str_append_scalar`
/// writes it, an enum case `Class::Case`, an array by its keys where it
/// is no list, and any other object `object(Class)`, as GH-15902 has it.
fn value_default_string(req: &mut Request, value: &Value) -> String {
    match value.as_ref() {
        ValueRef::Null => String::from("NULL"),
        ValueRef::Bool(b) => String::from(if b { "true" } else { "false" }),
        ValueRef::Int(n) => n.to_string(),
        ValueRef::Float(f) => default_value_string(req, &ConstExpr::Double(f)),
        ValueRef::Str(s) => format!("'{}'", escaped(s.as_bytes())),
        ValueRef::Array(array) => {
            let is_list = (0..)
                .zip(walk::rows(req, array))
                .all(|(i, (key, _))| key == Key::Int(i));
            let mut out = String::from("[");
            let mut first = true;
            // `zend_ast_export_zval`'s `ZEND_HASH_FOREACH_KEY_VAL`
            // (`Z/Zend/zend_ast.c:1854`), which no
            // code of the program reaches; the walk lends each row while the
            // value's text is made.
            let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                if !is_list {
                    match row.key() {
                        Key::Str(s) => {
                            write!(out, "'{}'", escaped(s.as_bytes())).ok();
                        }
                        Key::Int(n) => out.push_str(&n.to_string()),
                    }
                    out.push_str(" => ");
                }
                out.push_str(&value_default_string(req, row.value()));
                Ok(ControlFlow::Continue(()))
            });
            match walked {
                // The body answers no throwable.
                Ok(_) | Err(_) => {}
            }
            out.push(']');
            out
        }
        ValueRef::Object(object) => {
            let class = class_name_of(req, object.class());
            let class = String::from_utf8_lossy(&class).into_owned();
            let is_enum = req
                .class_decl(object.class())
                .is_some_and(|decl| decl.kind == ClassKind::Enum);
            match object.prop(req, b"name").filter(|_| is_enum) {
                Some(case) => format!(
                    "{class}::{}",
                    String::from_utf8_lossy(case.as_str_bytes().unwrap_or_default())
                ),
                None => format!("object({class})"),
            }
        }
        _ => String::from("<default>"),
    }
}

/// `smart_str_append_double(str, num, EG(precision), true)`, which
/// `smart_str_append_scalar` renders a float default with: the number as
/// `precision` prints it, with `.0` after a finite one that prints no
/// point, for a user function's default and a native's alike.
///
/// ```php
/// function f(float $t = 1.0, $u = 2.50, $v = -0.0, $w = 1e100, $x = 0.1, $y = [1.0]) {}
/// foreach ((new ReflectionFunction('f'))->getParameters() as $p) echo $p, "\n";
/// ```
/// ```expect
/// Parameter #0 [ <optional> float $t = 1.0 ]
/// Parameter #1 [ <optional> $u = 2.5 ]
/// Parameter #2 [ <optional> $v = -0.0 ]
/// Parameter #3 [ <optional> $w = 1.0E+100 ]
/// Parameter #4 [ <optional> $x = 0.1 ]
/// Parameter #5 [ <optional> $y = [1.0] ]
/// ```
fn double_string(req: &mut Request, f: f64) -> String {
    let Ok(mut text) = php_ext::coerce::with_string(req, &Value::float(f), |printed| {
        String::from_utf8_lossy(printed).into_owned()
    }) else {
        return String::new();
    };
    if f.is_finite() && !text.contains('.') {
        text.push_str(".0");
    }
    text
}

/// An element of an expression still to evaluate as `zend_ast_export`
/// writes it: an array the compiler folded as `zend_ast_export_zval`
/// writes the zval, every key before its value whatever the array's
/// shape, and anything else as [`default_value_string`] writes it.
///
/// ```php
/// function f($a = [FOO, [1, 2], ['k' => 'v', 1.5]], $b = [1, 'k' => [2, 3]], $c = [true, null, 1.0, "q'x"]) {}
/// foreach ((new ReflectionFunction('f'))->getParameters() as $p) echo $p, "\n";
/// class A { public $a = [1, "k" => 2, 3]; public $b = [1, 2]; public $c = [FOO, 2 => X, [5]]; }
/// foreach ((new ReflectionClass("A"))->getProperties() as $p) echo $p;
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = [FOO, [0 => 1, 1 => 2], ['k' => 'v', 0 => 1.5]] ]
/// Parameter #1 [ <optional> $b = [0 => 1, 'k' => [2, 3]] ]
/// Parameter #2 [ <optional> $c = [true, NULL, 1.0, 'q'x'] ]
/// Property [ public $a = [0 => 1, 'k' => 2, 1 => 3] ]
/// Property [ public $b = [1, 2] ]
/// Property [ public $c = [FOO, 2 => X, [0 => 5]] ]
/// ```
fn exported(req: &mut Request, expr: &ConstExpr) -> String {
    match expr {
        ConstExpr::ImmutableArray(_) => folded_array(req, expr, exported_value),
        _ => default_value_string(req, expr),
    }
}

/// `zend_ast_export_zval` over a value of an array the compiler folded:
/// an array with every key written, `'key' => ` or the number, and a
/// scalar as [`value_default_string`] writes it.
fn exported_value(req: &mut Request, value: &Value) -> String {
    let ValueRef::Array(array) = value.as_ref() else {
        return value_default_string(req, value);
    };
    let mut out = String::from("[");
    let mut first = true;
    // As [`value_default_string`] walks one.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        if !first {
            out.push_str(", ");
        }
        first = false;
        match row.key() {
            Key::Str(s) => {
                write!(out, "'{}' => ", escaped(s.as_bytes())).ok();
            }
            Key::Int(n) => {
                write!(out, "{n} => ").ok();
            }
        }
        out.push_str(&exported_value(req, row.value()));
        Ok(ControlFlow::Continue(()))
    });
    match walked {
        // The body answers no throwable.
        Ok(_) | Err(_) => {}
    }
    out.push(']');
    out
}

/// The array the compiler folded, `expr`, as `write` renders its value;
/// reading it takes no owner and raises nothing.
fn folded_array(
    req: &mut Request,
    expr: &ConstExpr,
    write: fn(&mut Request, &Value) -> String,
) -> String {
    match eval_const_expr(req, expr, None) {
        Ok(value) => {
            let written = write(req, &value);
            value.give_up(req);
            written
        }
        Err(_) => String::new(),
    }
}

/// `format_default_value` over a declared default: `smart_str_append_scalar`
/// for a scalar, `NULL`, `true`, `false`, the number, or the string in
/// single quotes with `smart_str_append_escaped`'s escapes; an array as
/// `[a, b]` or `['k' => v]`; a constant's name as `zend_ast_export`
/// writes an unevaluated expression, a class constant's class after the
/// mark compilation left on it:
///
/// ```php
/// namespace App { function f($a = Bar::BAZ) {} }
/// namespace { function g($a = N\Bar::BAZ) {} echo (new ReflectionFunction('App\f'))->getParameters()[0], "\n", (new ReflectionFunction('g'))->getParameters()[0], "\n"; }
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = \App\Bar::BAZ ]
/// Parameter #0 [ <optional> $a = N\Bar::BAZ ]
/// ```
fn default_value_string(req: &mut Request, expr: &ConstExpr) -> String {
    match expr {
        ConstExpr::Null => String::from("NULL"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long { value, .. } => value.to_string(),
        ConstExpr::Double(f) => double_string(req, *f),
        ConstExpr::Str(s) => format!("'{}'", escaped(s)),
        // The array the compiler folded is the value it is, whose keys
        // print unless it is a list.
        ConstExpr::ImmutableArray(_) => folded_array(req, expr, value_default_string),
        // `zend_ast_export`'s `ZEND_AST_ARRAY`, an array some element of
        // which is still to evaluate: a key where one is written, and
        // each element as the export writes it ([`exported`]).
        ConstExpr::Array { elements, .. } => {
            let mut out = String::from("[");
            for (i, (key, value)) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match key {
                    Some(ConstExpr::Str(s)) => {
                        write!(out, "'{}' => ", escaped(s)).ok();
                    }
                    Some(key) => {
                        write!(out, "{} => ", exported(req, key)).ok();
                    }
                    None => {}
                }
                out.push_str(&exported(req, value));
            }
            out.push(']');
            out
        }
        ConstExpr::Constant { name, .. } => {
            String::from_utf8_lossy(&name.resolved().0).into_owned()
        }
        // `zend_ast_export_ns_name`: the mark compilation left on the
        // class's name before it.
        ConstExpr::ClassConstant {
            class,
            class_kind,
            name,
            ..
        } => {
            let mark = match class_kind {
                NameKind::NotFullyQualified => "",
                NameKind::FullyQualified => "\\",
                NameKind::Relative => "namespace\\",
            };
            format!(
                "{mark}{}::{}",
                String::from_utf8_lossy(&class.0),
                String::from_utf8_lossy(&name.0)
            )
        }
        ConstExpr::ClassName { class, .. } => {
            format!("{}::class", String::from_utf8_lossy(&class.0))
        }
        // `zend_ast_export_name` over a name that is no string: the
        // expression written between the braces, without them.
        ConstExpr::DynamicClassConstant {
            class,
            class_kind,
            name,
            ..
        } => {
            let mark = match class_kind {
                NameKind::NotFullyQualified => "",
                NameKind::FullyQualified => "\\",
                NameKind::Relative => "namespace\\",
            };
            format!(
                "{mark}{}::{}",
                String::from_utf8_lossy(&class.0),
                ast_export(req, name, 0)
            )
        }
        ConstExpr::Closure { .. }
        | ConstExpr::FunctionCallable { .. }
        | ConstExpr::MethodCallable { .. } => callable_string(expr),
        ConstExpr::Unary { .. }
        | ConstExpr::Binary { .. }
        | ConstExpr::Dim { .. }
        | ConstExpr::Prop { .. }
        | ConstExpr::Conditional { .. }
        | ConstExpr::Cast { .. }
        | ConstExpr::New { .. } => ast_export(req, expr, 0),
        _ => String::from("<default>"),
    }
}

/// `zend_ast_export` over the three nodes 8.5 makes a `Closure` of: the
/// closure as `Closure(` its function's name `)`, and a first-class
/// callable as its name after the mark `zend_ast_export_ns_name` puts
/// before it and `(...)`.
fn callable_string(expr: &ConstExpr) -> String {
    match expr {
        // `ZEND_AST_OP_ARRAY`: `Closure(` the function's name `)`.
        ConstExpr::Closure { name, .. } => {
            format!("Closure({})", String::from_utf8_lossy(&name.0))
        }
        // `ZEND_AST_CALL` over `ZEND_AST_CALLABLE_CONVERT`, the name as
        // `zend_ast_export_ns_name` writes it.
        ConstExpr::FunctionCallable {
            name, name_kind, ..
        } => {
            let mark = if *name_kind == NameKind::FullyQualified {
                "\\"
            } else {
                ""
            };
            format!("{mark}{}(...)", String::from_utf8_lossy(&name.0))
        }
        // `ZEND_AST_STATIC_CALL`, its class marked as a `new`'s is by
        // `zend_compile_const_expr_class_reference`.
        ConstExpr::MethodCallable { class, method, .. } => format!(
            "{}::{}(...)",
            new_class_name(&class.0),
            String::from_utf8_lossy(&method.0)
        ),
        _ => String::new(),
    }
}

/// `zend_ast_export_ns_name` over the class a constant expression's
/// `new` names: `\` before a name the compiler resolved, which
/// `zend_compile_const_expr_class_reference` marks `ZEND_NAME_FQ`, and
/// `self` and `parent` as written.
fn new_class_name(class: &[u8]) -> String {
    let written = String::from_utf8_lossy(class);
    if class.eq_ignore_ascii_case(b"self") || class.eq_ignore_ascii_case(b"parent") {
        return written.into_owned();
    }
    format!("\\{written}")
}

/// `zend_ast_export_var` over a property's name: a string that is a
/// valid name as it stands, and anything else in braces.
fn property_name(req: &mut Request, name: &ConstExpr) -> String {
    if let ConstExpr::Str(bytes) = name {
        let valid = bytes
            .first()
            .is_some_and(|first| first.is_ascii_alphabetic() || *first == b'_' || *first >= 0x80)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte >= 0x80);
        // `zend_ast_export_name`: a string as it stands, in braces
        // where it is no valid name.
        let written = String::from_utf8_lossy(bytes);
        if valid {
            return written.into_owned();
        }
        return format!("{{{written}}}");
    }
    format!("{{{}}}", ast_export(req, name, 0))
}

/// `zend_ast_export_ex` over an operator of a constant expression, the
/// text `format_default_value` prints for a default the compiler kept
/// unevaluated: `BINARY_OP` and `PREFIX_OP` with php's priorities, a
/// side whose operator binds looser than its place wrapped in
/// parentheses, and every other node as [`default_value_string`] writes
/// it.
///
/// ```php
/// function m($a = U + 1, $b = -V, $c = W * (Y + 1), $d = Z . 'x') {}
/// foreach ((new ReflectionFunction('m'))->getParameters() as $p) { echo $p, "\n"; }
/// ```
/// ```expect
/// Parameter #0 [ <optional> $a = U + 1 ]
/// Parameter #1 [ <optional> $b = -V ]
/// Parameter #2 [ <optional> $c = W * (Y + 1) ]
/// Parameter #3 [ <optional> $d = Z . 'x' ]
/// ```
///
/// A `new` with its class written as resolved, a conditional, a `??`, a
/// cast, a dimension and a property, `?->` and braces kept:
///
/// ```php
/// class Foo { const BAR = 1; }
/// function g($d = new Foo(1, x: new Foo), $g = X ? 1 : Foo::BAR, $h = Foo::BAR ?? 2, $i = (int) Foo::BAR, $j = Foo::BAR['x'], $k = FOO?->x, $l = FOO->{'a b'}) {}
/// foreach ((new ReflectionFunction('g'))->getParameters() as $p) { echo $p, "\n"; }
/// ```
/// ```expect
/// Parameter #0 [ <optional> $d = new \Foo(1, x: new \Foo()) ]
/// Parameter #1 [ <optional> $g = X ? 1 : Foo::BAR ]
/// Parameter #2 [ <optional> $h = Foo::BAR ?? 2 ]
/// Parameter #3 [ <optional> $i = (int)Foo::BAR ]
/// Parameter #4 [ <optional> $j = Foo::BAR['x'] ]
/// Parameter #5 [ <optional> $k = FOO?->x ]
/// Parameter #6 [ <optional> $l = FOO->{a b} ]
/// ```
fn ast_export(req: &mut Request, expr: &ConstExpr, priority: u32) -> String {
    let (text, own) = match expr {
        ConstExpr::Unary { op, operand, .. } => {
            let operand = ast_export(req, operand, 241);
            (format!("{}{operand}", String::from_utf8_lossy(&op.0)), 240)
        }
        ConstExpr::Binary { op, lhs, rhs, .. } => {
            let (own, left, right) = match op.0.as_ref() {
                b"**" => (250, 251, 250),
                b"*" | b"/" | b"%" => (210, 210, 211),
                b"+" | b"-" => (200, 200, 201),
                b"<<" | b">>" => (190, 190, 191),
                b"." => (185, 185, 186),
                b"<" | b"<=" | b">" | b">=" | b"<=>" => (180, 181, 181),
                b"==" | b"!=" | b"===" | b"!==" => (170, 171, 171),
                b"&" => (160, 160, 161),
                b"^" => (150, 150, 151),
                b"|" => (140, 140, 141),
                b"&&" => (130, 130, 131),
                b"||" => (120, 120, 121),
                b"??" => (110, 111, 110),
                _ => (40, 40, 41),
            };
            let lhs = ast_export(req, lhs, left);
            let rhs = ast_export(req, rhs, right);
            (
                format!("{lhs} {} {rhs}", String::from_utf8_lossy(&op.0)),
                own,
            )
        }
        ConstExpr::Conditional {
            cond,
            then,
            otherwise,
        } => {
            let cond = ast_export(req, cond, 100);
            let middle = match then {
                Some(then) => format!(" ? {} : ", ast_export(req, then, 101)),
                None => String::from(" ?: "),
            };
            let otherwise = ast_export(req, otherwise, 101);
            (format!("{cond}{middle}{otherwise}"), 100)
        }
        ConstExpr::Cast { to, operand, .. } => {
            let written = match to {
                18 => "(bool)",
                4 => "(int)",
                5 => "(float)",
                6 => "(string)",
                7 => "(array)",
                _ => "(object)",
            };
            let operand = ast_export(req, operand, 241);
            (format!("{written}{operand}"), 240)
        }
        // The primary expressions, which no priority wraps.
        ConstExpr::Dim { container, dim, .. } => {
            let container = ast_export(req, container, 260);
            let dim = ast_export(req, dim, 0);
            return format!("{container}[{dim}]");
        }
        ConstExpr::Prop {
            object,
            name,
            nullsafe,
            ..
        } => {
            let object = ast_export(req, object, 0);
            let arrow = if *nullsafe { "?->" } else { "->" };
            let name = property_name(req, name);
            return format!("{object}{arrow}{name}");
        }
        ConstExpr::New {
            class, arguments, ..
        } => {
            let mut text = format!("new {}(", new_class_name(&class.0));
            for (at, (name, value)) in arguments.iter().enumerate() {
                if at > 0 {
                    text.push_str(", ");
                }
                if let Some(name) = name {
                    write!(text, "{}: ", String::from_utf8_lossy(&name.0)).ok();
                }
                text.push_str(&ast_export(req, value, 0));
            }
            text.push(')');
            return text;
        }
        leaf => return exported(req, leaf),
    };
    if priority > own {
        format!("({text})")
    } else {
        text
    }
}

/// `smart_str_append_escaped`: a backslash, a byte below 32 and one
/// above 126 escaped, the C escapes for the seven that have one and
/// `\xHH` for the rest.
fn escaped(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0C => out.push_str("\\f"),
            0x0B => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1B => out.push_str("\\e"),
            32..=126 => out.push(char::from(byte)),
            _ => {
                write!(out, "\\x{byte:02X}").ok();
            }
        }
    }
    out
}

/// One method of a linked class's function table, `zend_function` as
/// `zend_do_inheritance` left it: the declaration, `common.scope`, the
/// class that declared it, and the row of the engine's table it was
/// found at, which `common.prototype` is read off
/// ([`Request::row_prototype`]), or the `__invoke` php makes for a
/// `Closure`, which no table holds; nothing held for a property's hook.
#[derive(Clone)]
struct LinkedMethod {
    decl: MethodDecl,
    scope: ClassId,
    held: Option<HeldMethod>,
}

impl LinkedMethod {
    /// The method a row of a function table is, its declaration copied
    /// out of the engine's.
    fn of_row(req: &Request, row: MethodRow) -> Option<LinkedMethod> {
        Some(LinkedMethod {
            decl: req.row_method(row)?.clone(),
            scope: row.scope(),
            held: Some(HeldMethod::Row(row)),
        })
    }

    /// The method a reflector holds, read back.
    fn of_held(req: &mut Request, held: HeldMethod) -> Option<LinkedMethod> {
        match held {
            HeldMethod::Row(row) => LinkedMethod::of_row(req, row),
            HeldMethod::Invoke(function) => closure_invoke(req, function),
        }
    }

    /// The row of the function table the method is, none for a hook
    /// and for a closure's `__invoke`.
    fn row(&self) -> Option<MethodRow> {
        match self.held {
            Some(HeldMethod::Row(row)) => Some(row),
            Some(HeldMethod::Invoke(_)) | None => None,
        }
    }

    /// `ZEND_ACC_CALL_VIA_HANDLER`: a function no table holds, made on
    /// the lookup, which is a closure's `__invoke`
    /// ([`closure_invoke`]); its `module` is null, so it renders as
    /// `<internal>` with no extension.
    fn via_handler(&self) -> bool {
        matches!(self.held, Some(HeldMethod::Invoke(_)))
    }
}

/// `is_closure_invoke(ce, lcname)`: whether a lowered method name of
/// `class` is the `__invoke` php makes for a `Closure` on the lookup.
fn is_closure_invoke(req: &mut Request, class: ClassId, lowered: &[u8]) -> bool {
    lowered == b"__invoke" && lookup_class(req, b"Closure").ok().flatten() == Some(class)
}

/// A method of `class` by name, `zend_hash_find_ptr(&ce->function_table,
/// lcname)` over the linked table, and for `Closure`'s `__invoke`, which
/// no table holds, the method php makes on the lookup over `closure`,
/// or over a fresh one where none is given ([`closure_invoke`]).
fn method_or_invoke(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
    closure: Option<PhpObject>,
) -> Option<LinkedMethod> {
    let found = table_method(req, class, name);
    if found.is_some() || !is_closure_invoke(req, class, &name.to_ascii_lowercase()) {
        return found;
    }
    let function = closure.and_then(|closure| closure.closure(req).map(|c| c.function));
    closure_invoke(req, function)
}

/// `zend_hash_find_ptr(&ce->function_table, lcname)` alone: the
/// method of that name in the engine's linked table of `class`, any
/// case, with no `__invoke` made for a `Closure`. The table is the
/// link's, rows in php's order: the class's own methods, then each of
/// the parent's it does not declare, a private one too, then the
/// traits' abstract methods, then each interface's.
///
/// ```php
/// interface I { function i(); }
/// abstract class B { function b() {} abstract function a(); }
/// abstract class C extends B implements I { function A() {} protected function hidden() {} }
/// $m = new ReflectionMethod("C", "I");
/// $n = (new ReflectionClass("C"))->getMethod("B");
/// $o = new ReflectionMethod("C", "a");
/// var_dump($m->getName(), $m->class, $m->isAbstract(), $n->class, $n->isStatic(), $o->getPrototype()->class);
/// var_dump(array_map(fn ($m) => $m->class . "::" . $m->name, (new ReflectionClass("C"))->getMethods()));
/// var_dump((new ReflectionClass("C"))->hasMethod("HIDDEN"), (new ReflectionMethod("C", "hidden"))->isProtected());
/// ```
/// ```expect
/// string(1) "i"
/// string(1) "I"
/// bool(true)
/// string(1) "B"
/// bool(false)
/// string(1) "B"
/// array(4) {
///   [0]=>
///   string(4) "C::A"
///   [1]=>
///   string(9) "C::hidden"
///   [2]=>
///   string(4) "B::b"
///   [3]=>
///   string(4) "I::i"
/// }
/// bool(true)
/// bool(true)
/// ```
fn table_method(req: &Request, class: ClassId, name: &[u8]) -> Option<LinkedMethod> {
    LinkedMethod::of_row(req, req.method_row(class, name)?)
}

/// Every method of the engine's linked table of `class` in its order
/// ([`table_method`]), each declaration copied out, which a rendering
/// walks while it writes.
fn table_methods(req: &Request, class: ClassId) -> Vec<LinkedMethod> {
    req.method_rows(class)
        .filter_map(|row| LinkedMethod::of_row(req, row))
        .collect()
}

/// `zend_get_closure_invoke_method`: `Closure::__invoke` over one
/// closure's own signature, public, an instance method, internal with
/// no extension, no doc comment and no site, the return type and the
/// by-reference bit kept. Its `arg_info` is the closure's, which a
/// native reads as `zend_internal_arg_info` with no `default_value`, so
/// a written default is one it does not show. `None` is the closure
/// `object_init_ex(zend_ce_closure)` makes, which holds no function and
/// so declares nothing.
///
/// ```php
/// $c = function (int $a, $b = 5): int { return $a; };
/// $m = new ReflectionMethod($c, '__invoke');
/// echo $m, (new ReflectionClass('Closure'))->getMethod('__invoke');
/// var_dump($m->isInternal(), $m->getFileName(), $m->getNumberOfRequiredParameters(), $m->invoke($c, 3), (new ReflectionObject($c))->hasMethod('__invoke'), count((new ReflectionObject($c))->getMethods()));
/// try { new ReflectionMethod('Closure', '__invoke'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Method [ <internal> public method __invoke ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $a ]
///     Parameter #1 [ <optional> $b = <default> ]
///   }
///   - Return [ int ]
/// }
/// Method [ <internal> public method __invoke ] {
/// }
/// bool(true)
/// bool(false)
/// int(1)
/// int(3)
/// bool(true)
/// int(7)
/// Method Closure::__invoke() does not exist
/// ```
fn closure_invoke(req: &mut Request, closure: Option<Function>) -> Option<LinkedMethod> {
    let scope = lookup_class(req, b"Closure").ok().flatten()?;
    let empty = || FunctionDecl {
        name: Name::default(),
        parameters: Vec::new(),
        return_type: None,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name::default(),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    };
    let mut function = closure
        .and_then(|closure| req.function_decl(closure).cloned())
        .unwrap_or_else(empty);
    function.name = Name(b"__invoke".as_slice().into());
    function.file = Name::default();
    (function.line, function.end_line) = (0, 0);
    function.doc_comment = None;
    function.is_generator = false;
    function
        .attributes
        .retain(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"));
    for parameter in &mut function.parameters {
        if let ParameterDefault::Expr(_) = parameter.default {
            parameter.default = ParameterDefault::Unshown;
        }
    }
    Some(LinkedMethod {
        decl: MethodDecl {
            function,
            visibility: Visibility::Public,
            is_instance: true,
            is_abstract: false,
            is_final: false,
            tentative_return: false,
        },
        scope,
        held: Some(HeldMethod::Invoke(closure)),
    })
}

/// `ZEND_ACC_CTOR`: the method named `__construct`.
fn is_constructor(method: &MethodDecl) -> bool {
    method.function.name.0.eq_ignore_ascii_case(b"__construct")
}

/// `CE_CONSTANTS_TABLE(ce)`: the rows of the engine's linked record in
/// php's order ([`Request::constants_in_order`]), the class's own first
/// for a user class and last for an internal one, each declaration
/// copied out beside the class that declared it, for a walk that
/// evaluates as it goes.
///
/// ```php
/// interface I { const IC = 1; } class P implements I { const PC = 2; private const PP = 3; } class C extends P { const CC = 4; }
/// foreach (['C', 'RecursiveTreeIterator', 'DateTimeImmutable'] as $c) echo implode(', ', array_keys((new ReflectionClass($c))->getConstants())), "\n";
/// ```
/// ```expect
/// CC, PC, IC
/// LEAVES_ONLY, SELF_FIRST, CHILD_FIRST, CATCH_GET_CHILD, BYPASS_CURRENT, BYPASS_KEY, PREFIX_LEFT, PREFIX_MID_HAS_NEXT, PREFIX_MID_LAST, PREFIX_END_HAS_NEXT, PREFIX_END_LAST, PREFIX_RIGHT
/// ATOM, COOKIE, ISO8601, ISO8601_EXPANDED, RFC822, RFC850, RFC1036, RFC1123, RFC7231, RFC2822, RFC3339, RFC3339_EXTENDED, RSS, W3C
/// ```
fn constant_rows(req: &Request, class: ClassId) -> Vec<(ClassConstDecl, ClassId)> {
    req.constants_in_order(class)
        .into_iter()
        .map(|(declaring, constant)| (constant.clone(), declaring))
        .collect()
}

/// `ce->properties_info` after linking: the class's own properties
/// and each of the parent's table the class does not redeclare
/// (`do_inherit_property`), a private one among them as the shadow
/// `_class_string` skips, each with the class that declared it. A user
/// class has its own first, compiled into the table before
/// `zend_do_inheritance` appends the rows of the parent's table it
/// does not hold, in that table's order, so a grandparent's rows come
/// after the parent's; an internal class has the parent's first,
/// `zend_register_internal_class_ex` inheriting before
/// `zend_declare_typed_property` adds its own, a redeclaration moving
/// to the end, which is why `ReflectionMethod` lists `$name` before
/// `$class`.
///
/// ```php
/// class A { static $a1 = 1; static $c = 1; } class B extends A { static $c = 2; static $b1 = 2; } class C extends B { static $c = 3; }
/// print_r((new ReflectionClass('C'))->getDefaultProperties());
/// ```
/// ```expect
/// Array
/// (
///     [c] => 3
///     [b1] => 2
///     [a1] => 1
/// )
/// ```
fn linked_properties(req: &mut Request, class: ClassId) -> Vec<(PropertyDecl, ClassId)> {
    req.property_info_rows(class)
        .filter_map(|row| Some((req.row_property_info(row)?.clone(), row.declaring())))
        .collect()
}

/// `_function_string` with no scope: `Function [ <internal:ext>
/// function name ] {`, its parameters and its return row, each line
/// under `indent`. A function with the `Deprecated` attribute carries
/// `, deprecated` after `<internal`, as `ZEND_ACC_DEPRECATED` does.
fn function_string(req: &mut Request, function: Function, indent: &str) -> String {
    let Some(decl) = req.function_decl(function).cloned() else {
        return String::new();
    };
    let module = req
        .function_extension(function)
        .and_then(|e| req.extension_name(e))
        .map(str::to_owned);
    let mut out = String::new();
    function_header(&mut out, &decl, module.as_deref(), Header::Function, indent);
    let arg_info = ArgInfo::of(&decl);
    function_body(req, &mut out, &decl, false, arg_info, &[], indent);
    out
}

/// `_function_string` over a closure, `ZEND_ACC_CLOSURE` set: the
/// function it holds, with `_function_closure_string`'s `Bound
/// Variables` block over the names its table holds a value for, in the
/// table's order, before the parameters.
///
/// ```php
/// $a = 1; $b = 2;
/// echo new ReflectionFunction(function ($x) use ($a, &$b) {}), new ReflectionFunction(fn () => 1);
/// ```
/// ```expect
/// Closure [ <user> function {closure:%s:3} ] {
///   @@ %s 3 - 3
///
///   - Bound Variables [2] {
///       Variable #0 [ $a ]
///       Variable #1 [ $b ]
///   }
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $x ]
///   }
/// }
/// Closure [ <user> function {closure:%s:3} ] {
///   @@ %s 3 - 3
/// }
/// ```
///
/// A closure made of a named function is a closure too:
///
/// ```php
/// function f(int $a) {}
/// echo new ReflectionFunction(f(...)), new ReflectionFunction(ucfirst(...));
/// ```
/// ```expect
/// Closure [ <user> function f ] {
///   @@ %s 2 - 2
///
///   - Parameters [1] {
///     Parameter #0 [ <required> int $a ]
///   }
/// }
/// Closure [ <internal:standard> function ucfirst ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// ```
fn closure_string(req: &mut Request, closure: PhpObject, indent: &str) -> Result<String, Thrown> {
    let Some(decl) = signature::closure_decl(req, closure) else {
        return Ok(String::new());
    };
    let mut bound: Vec<Vec<u8>> = Vec::new();
    for (name, _) in closure.closure_bound(req)? {
        bound.push(name.as_bytes().to_vec());
        name.give_up(req);
    }
    let module = closure
        .closure(req)
        .and_then(|info| req.function_extension(info.function))
        .and_then(|e| req.extension_name(e))
        .map(str::to_owned);
    let mut out = String::new();
    function_header(&mut out, &decl, module.as_deref(), Header::Closure, indent);
    let arg_info = ArgInfo::of(&decl);
    function_body(req, &mut out, &decl, false, arg_info, &bound, indent);
    Ok(out)
}

/// Which `arg_info` a function carries, which decides whether
/// `_function_parameter_string` prints anything: a native's
/// `zend_internal_arg_info`, whose first row is the return and so is
/// never null, or the compiler's `zend_arg_info`, which
/// `zend_compile_params` leaves null for a function with neither a
/// parameter nor a return type. A closure's `__invoke` carries its
/// closure's, the user kind (`ZEND_ACC_USER_ARG_INFO`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgInfo {
    Native,
    User,
}

impl ArgInfo {
    /// The kind a declaration's site says: a native has no file.
    fn of(decl: &FunctionDecl) -> ArgInfo {
        if decl.file.0.is_empty() {
            ArgInfo::Native
        } else {
            ArgInfo::User
        }
    }

    /// Whether php's pointer is null for `decl`.
    fn is_null(self, decl: &FunctionDecl) -> bool {
        self == ArgInfo::User && decl.parameters.is_empty() && decl.return_type.is_none()
    }
}

/// `_function_string` with a scope, a method of `ce`: `Method [
/// <internal:ext, inherits S, overwrites O, prototype P, ctor> ... method
/// name ] {` with the modifier and visibility words between where the
/// declaration has them, then the
/// parameters and the return row, `Tentative return` where the
/// declaration carries the bit. `inherits` names the method's scope
/// when it is not `ce`; `overwrites` names the scope of the parent's
/// method of the same name when the method is `ce`'s own and the
/// parent's is not private; `prototype` names the prototype's scope.
fn method_string(req: &mut Request, method: &LinkedMethod, ce: ClassId, indent: &str) -> String {
    // `fptr->internal_function.module`: the function the row is names
    // it, so a native trait's method copied into a user class prints
    // the trait's extension; a function made on the lookup has none.
    let module = method
        .row()
        .and_then(|row| req.row_function(row))
        .and_then(|function| req.function_extension(function))
        .and_then(|extension| req.extension_name(extension))
        .map(str::to_owned);
    let mut words: Vec<String> = Vec::new();
    if method.scope != ce {
        let scope = class_name_of(req, method.scope);
        words.push(format!("inherits {}", String::from_utf8_lossy(&scope)));
    } else if let Some(parent) = class_chain(req, ce).get(1).copied() {
        let overwrites = req
            .method_row(parent, &method.decl.function.name.0)
            .filter(|row| row.scope() != method.scope)
            .filter(|row| {
                req.row_method(*row)
                    .is_some_and(|m| m.visibility != Visibility::Private)
            })
            .map(MethodRow::scope);
        if let Some(overwritten) = overwrites {
            let scope = class_name_of(req, overwritten);
            words.push(format!("overwrites {}", String::from_utf8_lossy(&scope)));
        }
    }
    if let Some(prototype) = method.row().and_then(|row| req.row_prototype(row)) {
        let scope = class_name_of(req, prototype);
        words.push(format!("prototype {}", String::from_utf8_lossy(&scope)));
    }
    if is_constructor(&method.decl) {
        words.push(String::from("ctor"));
    }
    let mut out = String::new();
    function_header(
        &mut out,
        &method.decl.function,
        module.as_deref(),
        Header::Method(&method.decl, &words),
        indent,
    );
    let arg_info = if method.via_handler() {
        ArgInfo::User
    } else {
        ArgInfo::of(&method.decl.function)
    };
    function_body(
        req,
        &mut out,
        &method.decl.function,
        method.decl.tentative_return,
        arg_info,
        &[],
        indent,
    );
    out
}

/// What `_function_string` renders, which decides its first word: a
/// function by name, a closure, `ZEND_ACC_CLOSURE` set, whether the
/// program wrote it or `zend_create_fake_closure` made it of a named
/// function, or a method of a class with its inheritance words.
#[derive(Clone, Copy)]
enum Header<'a> {
    Function,
    Closure,
    Method(&'a MethodDecl, &'a [String]),
}

/// The first line of `_function_string`: the doc comment where there
/// is one, `Function [` or `Method [`, `<internal:module` or `<user`
/// with `, deprecated` where the attribute is and a method's
/// inheritance words, then
/// the modifiers and the name.
fn function_header(
    out: &mut String,
    decl: &FunctionDecl,
    module: Option<&str>,
    header: Header<'_>,
    indent: &str,
) {
    let method = match header {
        Header::Method(method, words) => Some((method, words)),
        Header::Function | Header::Closure => None,
    };
    if let Some(doc) = &decl.doc_comment {
        out.push_str(indent);
        out.push_str(&String::from_utf8_lossy(doc));
        out.push('\n');
    }
    out.push_str(indent);
    // `fptr->common.fn_flags & ZEND_ACC_CLOSURE ? "Closure [ " :
    // (fptr->common.scope ? "Method [ " : "Function [ ")`
    // (`Z/ext/reflection/php_reflection.c:905`).
    out.push_str(match header {
        Header::Method(..) => "Method [ ",
        Header::Closure => "Closure [ ",
        Header::Function => "Function [ ",
    });
    let internal = decl.file.0.is_empty();
    out.push_str(if internal { "<internal" } else { "<user" });
    if decl
        .attributes
        .iter()
        .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    {
        out.push_str(", deprecated");
    }
    if internal && let Some(module) = module {
        write!(out, ":{module}").ok();
    }
    if let Some((_, words)) = method {
        for word in words {
            write!(out, ", {word}").ok();
        }
    }
    out.push_str("> ");
    if let Some((method, _)) = method {
        if method.is_abstract {
            out.push_str("abstract ");
        }
        if method.is_final {
            out.push_str("final ");
        }
        if !method.is_instance {
            out.push_str("static ");
        }
        write!(out, "{} method ", visibility_word(method.visibility)).ok();
    } else {
        out.push_str("function ");
    }
    if decl.returns_by_ref {
        out.push('&');
    }
    writeln!(out, "{} ] {{", String::from_utf8_lossy(&decl.name.0)).ok();
}

/// The rest of `_function_string`: a user function's `@@ file start -
/// end` line, the parameters, the return row and the closing brace.
/// php writes the `@@` row for a `ZEND_USER_FUNCTION` alone, off
/// `op_array.filename`, `line_start` and `line_end`, which are the file
/// and the lines of the `function` keyword and of the closing brace;
/// a native's file is empty and it carries no row, as
/// `_class_string` does for a class.
///
/// ```php
/// function f($a) {
///   return $a;
/// }
/// echo new ReflectionFunction('f');
/// ```
/// ```expect
/// Function [ <user> function f ] {
///   @@ %s 2 - 4
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $a ]
///   }
/// }
/// ```
fn function_body(
    req: &mut Request,
    out: &mut String,
    decl: &FunctionDecl,
    tentative: bool,
    arg_info: ArgInfo,
    bound: &[Vec<u8>],
    indent: &str,
) {
    if !decl.file.0.is_empty() {
        writeln!(
            out,
            "{indent}  @@ {} {} - {}",
            String::from_utf8_lossy(&decl.file.0),
            decl.line,
            decl.end_line
        )
        .ok();
    }
    let param_indent = format!("{indent}  ");
    if !bound.is_empty() {
        write!(
            out,
            "\n{param_indent}- Bound Variables [{}] {{\n",
            bound.len()
        )
        .ok();
        for (at, name) in bound.iter().enumerate() {
            writeln!(
                out,
                "{param_indent}    Variable #{at} [ ${} ]",
                String::from_utf8_lossy(name)
            )
            .ok();
        }
        writeln!(out, "{param_indent}}}").ok();
    }
    function_parameter_string(req, out, decl, arg_info, &param_indent);
    if let Some(returns) = &decl.return_type {
        let word = if tentative {
            "Tentative return"
        } else {
            "Return"
        };
        writeln!(out, "  {indent}- {word} [ {} ]", type_to_string(returns)).ok();
    }
    writeln!(out, "{indent}}}").ok();
}

/// `required_num_args`: the parameters up to the last one that is
/// required.
fn required_parameters(decl: &FunctionDecl) -> usize {
    decl.parameters
        .iter()
        .rposition(Parameter::is_required)
        .map_or(0, |at| at + 1)
}

/// `_parameter_string`: one `Parameter #N [ ... ]` row, the modifier
/// word, the type where the declaration has one, `&` for a by-reference
/// parameter, `...` for the variadic one, the name, and the default
/// after `=` where the row is optional and not variadic. php passes an
/// indent it does not write, so the row carries none.
fn parameter_string(
    req: &mut Request,
    parameter: &Parameter,
    offset: usize,
    required: bool,
    internal: bool,
) -> String {
    let mut out = format!("Parameter #{offset} [ ");
    out.push_str(if required {
        "<required> "
    } else {
        "<optional> "
    });
    if let Some(ty) = &parameter.ty {
        write!(out, "{} ", type_to_string(ty)).ok();
    }
    if parameter.by_ref {
        out.push('&');
    }
    if parameter.variadic {
        out.push_str("...");
    }
    write!(out, "${}", String::from_utf8_lossy(&parameter.name.0)).ok();
    // A native's default is the text `gen_stub.php` wrote into the arg
    // info, `<default>` where it wrote none; a user function's is the
    // zval `RECV_INIT` holds, which `format_default_value` prints and
    // `NULL` is the spelling of.
    if !required && !parameter.variadic {
        match (parameter.default.expr(), internal) {
            (Some(expr), true) => {
                write!(out, " = {}", const_expr_string(expr)).ok();
            }
            (None, true) => out.push_str(" = <default>"),
            (Some(expr), false) => {
                write!(out, " = {}", default_value_string(req, expr)).ok();
            }
            (None, false) => {}
        }
    }
    out.push_str(" ]");
    out
}

/// `_function_parameter_string`: `- Parameters [N] {` and one
/// [`parameter_string`] line per parameter, and nothing where php's
/// `arg_info` is null, which is a user function that declares neither
/// a parameter nor a return type (`zend_compile_params` allocates none
/// for it); a native's `arg_info` always carries its return row.
///
/// ```php
/// function f() {}
/// function g(): int { return 1; }
/// echo new ReflectionFunction('f'), new ReflectionFunction('g');
/// ```
/// ```expect
/// Function [ <user> function f ] {
///   @@ %s 2 - 2
/// }
/// Function [ <user> function g ] {
///   @@ %s 3 - 3
///
///   - Parameters [0] {
///   }
///   - Return [ int ]
/// }
/// ```
fn function_parameter_string(
    req: &mut Request,
    out: &mut String,
    decl: &FunctionDecl,
    arg_info: ArgInfo,
    indent: &str,
) {
    let required = required_parameters(decl);
    let internal = decl.file.0.is_empty();
    if arg_info.is_null(decl) {
        return;
    }
    out.push('\n');
    writeln!(out, "{indent}- Parameters [{}] {{", decl.parameters.len()).ok();
    for (offset, parameter) in decl.parameters.clone().iter().enumerate() {
        let row = parameter_string(req, parameter, offset, offset < required, internal);
        writeln!(out, "{indent}  {row}").ok();
    }
    writeln!(out, "{indent}}}").ok();
}

/// A native parameter's default as `gen_stub.php` writes it into
/// `zend_internal_arg_info.default_value`: what the stub generator's
/// printer makes of the expression, which for a scalar is the scalar as
/// written, for a string the bytes double-quoted with its escapes
/// ([`stub_escaped`]), a class constant as `Class::NAME`, and constants
/// joined by `|` as the stub joins them. A float is written with its
/// point, `1.0` and never `1`:
///
/// ```php
/// echo (new ReflectionFunction('curl_multi_select'))->getParameters()[1], "\n";
/// ```
/// ```expect
/// Parameter #1 [ <optional> float $timeout = 1.0 ]
/// ```
fn const_expr_string(expr: &php_ext::decl::ConstExpr) -> String {
    use php_ext::decl::ConstExpr;
    match expr {
        ConstExpr::Null => String::from("null"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long { value, radix } => radix.written(*value),
        ConstExpr::Double(f) if f.is_finite() && f.fract() == 0.0 => format!("{f:.1}"),
        ConstExpr::Double(f) => f.to_string(),
        ConstExpr::Str(s) => format!("\"{}\"", stub_escaped(s)),
        ConstExpr::Constant { name, .. } => {
            String::from_utf8_lossy(&name.resolved().0).into_owned()
        }
        // `DateTimeZone::ALL`, as `timezone_identifiers_list`'s stub writes it.
        ConstExpr::ClassConstant { class, name, .. } => format!(
            "{}::{}",
            String::from_utf8_lossy(&class.0),
            String::from_utf8_lossy(&name.0)
        ),
        // `SplFileObject::class`, as `setFileClass`'s stub writes it.
        ConstExpr::ClassName { class, .. } => {
            format!("{}::class", String::from_utf8_lossy(&class.0))
        }
        ConstExpr::Array { elements, .. } if elements.is_empty() => String::from("[]"),
        // `ENT_QUOTES | ENT_SUBSTITUTE | ENT_HTML401`: the stub's
        // operands with the operator between, as it wrote them.
        ConstExpr::Binary { op, lhs, rhs, .. } => format!(
            "{} {} {}",
            const_expr_string(lhs),
            String::from_utf8_lossy(&op.0),
            const_expr_string(rhs)
        ),
        _ => String::from("<default>"),
    }
}

/// php-parser's `escapeString` for a double-quoted string, which is how
/// `gen_stub.php` prints a string default: `addcslashes` over `\n`,
/// `\r`, `\t`, `\f`, `\v`, `$`, `"` and `\\`, then every other control
/// byte as `\x` and two lowercase hex digits, so `trim`'s
/// `" \n\r\t\v\0"` prints as `" \n\r\t\v\x00"`.
fn stub_escaped(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::new();
    for &byte in bytes {
        match byte {
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x0C => out.extend_from_slice(b"\\f"),
            0x0B => out.extend_from_slice(b"\\v"),
            b'$' | b'"' | b'\\' => out.extend_from_slice(&[b'\\', byte]),
            0x00..=0x08 | 0x0E..=0x1F => {
                out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
            }
            _ => out.push(byte),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

/// `ReflectionException` as the stub declares it.
#[php_class(extends(crate::Exception))]
pub(crate) struct ReflectionException;

#[php_class]
impl ReflectionException {}

/// `interface Reflector extends Stringable` as the stub declares it.
#[php_class(interface, extends(crate::Stringable))]
pub(crate) struct Reflector;

#[php_class]
impl Reflector {}

/// `ReflectionExtension` as the stub declares it.
#[php_class(implements(Reflector), not_serializable, size = 96)]
pub(crate) struct ReflectionExtension;

#[php_class]
impl ReflectionExtension {
    #[php(property)]
    const name: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

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
    fn __construct(req: &mut Request, this: This<Self>, name: PhpString) -> Result<(), Thrown> {
        let name = name.as_bytes().to_vec();
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
        let this = this.object();
        set_name(req, this, declared.as_bytes())?;
        Ok(())
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
    ///
    /// ```php
    /// echo new ReflectionExtension('tokenizer');
    /// ```
    /// ```expect
    /// Extension [ <persistent> extension #%N% tokenizer version 8.5.9 ] {
    ///
    ///   - Constants [154] {
    ///     Constant [ <persistent> int T_LNUMBER ] { 260 }
    ///     Constant [ <persistent> int T_DNUMBER ] { 261 }
    ///     Constant [ <persistent> int T_STRING ] { 262 }
    ///     Constant [ <persistent> int T_NAME_FULLY_QUALIFIED ] { 263 }
    ///     Constant [ <persistent> int T_NAME_RELATIVE ] { 264 }
    ///     Constant [ <persistent> int T_NAME_QUALIFIED ] { 265 }
    ///     Constant [ <persistent> int T_VARIABLE ] { 266 }
    ///     Constant [ <persistent> int T_INLINE_HTML ] { 267 }
    ///     Constant [ <persistent> int T_ENCAPSED_AND_WHITESPACE ] { 268 }
    ///     Constant [ <persistent> int T_CONSTANT_ENCAPSED_STRING ] { 269 }
    ///     Constant [ <persistent> int T_STRING_VARNAME ] { 270 }
    ///     Constant [ <persistent> int T_NUM_STRING ] { 271 }
    ///     Constant [ <persistent> int T_INCLUDE ] { 272 }
    ///     Constant [ <persistent> int T_INCLUDE_ONCE ] { 273 }
    ///     Constant [ <persistent> int T_EVAL ] { 274 }
    ///     Constant [ <persistent> int T_REQUIRE ] { 275 }
    ///     Constant [ <persistent> int T_REQUIRE_ONCE ] { 276 }
    ///     Constant [ <persistent> int T_LOGICAL_OR ] { 277 }
    ///     Constant [ <persistent> int T_LOGICAL_XOR ] { 278 }
    ///     Constant [ <persistent> int T_LOGICAL_AND ] { 279 }
    ///     Constant [ <persistent> int T_PRINT ] { 280 }
    ///     Constant [ <persistent> int T_YIELD ] { 281 }
    ///     Constant [ <persistent> int T_YIELD_FROM ] { 282 }
    ///     Constant [ <persistent> int T_INSTANCEOF ] { 283 }
    ///     Constant [ <persistent> int T_NEW ] { 284 }
    ///     Constant [ <persistent> int T_CLONE ] { 285 }
    ///     Constant [ <persistent> int T_EXIT ] { 286 }
    ///     Constant [ <persistent> int T_IF ] { 287 }
    ///     Constant [ <persistent> int T_ELSEIF ] { 288 }
    ///     Constant [ <persistent> int T_ELSE ] { 289 }
    ///     Constant [ <persistent> int T_ENDIF ] { 290 }
    ///     Constant [ <persistent> int T_ECHO ] { 291 }
    ///     Constant [ <persistent> int T_DO ] { 292 }
    ///     Constant [ <persistent> int T_WHILE ] { 293 }
    ///     Constant [ <persistent> int T_ENDWHILE ] { 294 }
    ///     Constant [ <persistent> int T_FOR ] { 295 }
    ///     Constant [ <persistent> int T_ENDFOR ] { 296 }
    ///     Constant [ <persistent> int T_FOREACH ] { 297 }
    ///     Constant [ <persistent> int T_ENDFOREACH ] { 298 }
    ///     Constant [ <persistent> int T_DECLARE ] { 299 }
    ///     Constant [ <persistent> int T_ENDDECLARE ] { 300 }
    ///     Constant [ <persistent> int T_AS ] { 301 }
    ///     Constant [ <persistent> int T_SWITCH ] { 302 }
    ///     Constant [ <persistent> int T_ENDSWITCH ] { 303 }
    ///     Constant [ <persistent> int T_CASE ] { 304 }
    ///     Constant [ <persistent> int T_DEFAULT ] { 305 }
    ///     Constant [ <persistent> int T_MATCH ] { 306 }
    ///     Constant [ <persistent> int T_BREAK ] { 307 }
    ///     Constant [ <persistent> int T_CONTINUE ] { 308 }
    ///     Constant [ <persistent> int T_GOTO ] { 309 }
    ///     Constant [ <persistent> int T_FUNCTION ] { 310 }
    ///     Constant [ <persistent> int T_FN ] { 311 }
    ///     Constant [ <persistent> int T_CONST ] { 312 }
    ///     Constant [ <persistent> int T_RETURN ] { 313 }
    ///     Constant [ <persistent> int T_TRY ] { 314 }
    ///     Constant [ <persistent> int T_CATCH ] { 315 }
    ///     Constant [ <persistent> int T_FINALLY ] { 316 }
    ///     Constant [ <persistent> int T_THROW ] { 317 }
    ///     Constant [ <persistent> int T_USE ] { 318 }
    ///     Constant [ <persistent> int T_INSTEADOF ] { 319 }
    ///     Constant [ <persistent> int T_GLOBAL ] { 320 }
    ///     Constant [ <persistent> int T_STATIC ] { 321 }
    ///     Constant [ <persistent> int T_ABSTRACT ] { 322 }
    ///     Constant [ <persistent> int T_FINAL ] { 323 }
    ///     Constant [ <persistent> int T_PRIVATE ] { 324 }
    ///     Constant [ <persistent> int T_PROTECTED ] { 325 }
    ///     Constant [ <persistent> int T_PUBLIC ] { 326 }
    ///     Constant [ <persistent> int T_PRIVATE_SET ] { 327 }
    ///     Constant [ <persistent> int T_PROTECTED_SET ] { 328 }
    ///     Constant [ <persistent> int T_PUBLIC_SET ] { 329 }
    ///     Constant [ <persistent> int T_READONLY ] { 330 }
    ///     Constant [ <persistent> int T_VAR ] { 331 }
    ///     Constant [ <persistent> int T_UNSET ] { 332 }
    ///     Constant [ <persistent> int T_ISSET ] { 333 }
    ///     Constant [ <persistent> int T_EMPTY ] { 334 }
    ///     Constant [ <persistent> int T_HALT_COMPILER ] { 335 }
    ///     Constant [ <persistent> int T_CLASS ] { 336 }
    ///     Constant [ <persistent> int T_TRAIT ] { 337 }
    ///     Constant [ <persistent> int T_INTERFACE ] { 338 }
    ///     Constant [ <persistent> int T_ENUM ] { 339 }
    ///     Constant [ <persistent> int T_EXTENDS ] { 340 }
    ///     Constant [ <persistent> int T_IMPLEMENTS ] { 341 }
    ///     Constant [ <persistent> int T_NAMESPACE ] { 342 }
    ///     Constant [ <persistent> int T_LIST ] { 343 }
    ///     Constant [ <persistent> int T_ARRAY ] { 344 }
    ///     Constant [ <persistent> int T_CALLABLE ] { 345 }
    ///     Constant [ <persistent> int T_LINE ] { 346 }
    ///     Constant [ <persistent> int T_FILE ] { 347 }
    ///     Constant [ <persistent> int T_DIR ] { 348 }
    ///     Constant [ <persistent> int T_CLASS_C ] { 349 }
    ///     Constant [ <persistent> int T_TRAIT_C ] { 350 }
    ///     Constant [ <persistent> int T_METHOD_C ] { 351 }
    ///     Constant [ <persistent> int T_FUNC_C ] { 352 }
    ///     Constant [ <persistent> int T_PROPERTY_C ] { 353 }
    ///     Constant [ <persistent> int T_NS_C ] { 354 }
    ///     Constant [ <persistent> int T_ATTRIBUTE ] { 355 }
    ///     Constant [ <persistent> int T_PLUS_EQUAL ] { 356 }
    ///     Constant [ <persistent> int T_MINUS_EQUAL ] { 357 }
    ///     Constant [ <persistent> int T_MUL_EQUAL ] { 358 }
    ///     Constant [ <persistent> int T_DIV_EQUAL ] { 359 }
    ///     Constant [ <persistent> int T_CONCAT_EQUAL ] { 360 }
    ///     Constant [ <persistent> int T_MOD_EQUAL ] { 361 }
    ///     Constant [ <persistent> int T_AND_EQUAL ] { 362 }
    ///     Constant [ <persistent> int T_OR_EQUAL ] { 363 }
    ///     Constant [ <persistent> int T_XOR_EQUAL ] { 364 }
    ///     Constant [ <persistent> int T_SL_EQUAL ] { 365 }
    ///     Constant [ <persistent> int T_SR_EQUAL ] { 366 }
    ///     Constant [ <persistent> int T_COALESCE_EQUAL ] { 367 }
    ///     Constant [ <persistent> int T_BOOLEAN_OR ] { 368 }
    ///     Constant [ <persistent> int T_BOOLEAN_AND ] { 369 }
    ///     Constant [ <persistent> int T_IS_EQUAL ] { 370 }
    ///     Constant [ <persistent> int T_IS_NOT_EQUAL ] { 371 }
    ///     Constant [ <persistent> int T_IS_IDENTICAL ] { 372 }
    ///     Constant [ <persistent> int T_IS_NOT_IDENTICAL ] { 373 }
    ///     Constant [ <persistent> int T_IS_SMALLER_OR_EQUAL ] { 374 }
    ///     Constant [ <persistent> int T_IS_GREATER_OR_EQUAL ] { 375 }
    ///     Constant [ <persistent> int T_SPACESHIP ] { 376 }
    ///     Constant [ <persistent> int T_SL ] { 377 }
    ///     Constant [ <persistent> int T_SR ] { 378 }
    ///     Constant [ <persistent> int T_INC ] { 379 }
    ///     Constant [ <persistent> int T_DEC ] { 380 }
    ///     Constant [ <persistent> int T_INT_CAST ] { 381 }
    ///     Constant [ <persistent> int T_DOUBLE_CAST ] { 382 }
    ///     Constant [ <persistent> int T_STRING_CAST ] { 383 }
    ///     Constant [ <persistent> int T_ARRAY_CAST ] { 384 }
    ///     Constant [ <persistent> int T_OBJECT_CAST ] { 385 }
    ///     Constant [ <persistent> int T_BOOL_CAST ] { 386 }
    ///     Constant [ <persistent> int T_UNSET_CAST ] { 387 }
    ///     Constant [ <persistent> int T_VOID_CAST ] { 388 }
    ///     Constant [ <persistent> int T_OBJECT_OPERATOR ] { 389 }
    ///     Constant [ <persistent> int T_NULLSAFE_OBJECT_OPERATOR ] { 390 }
    ///     Constant [ <persistent> int T_DOUBLE_ARROW ] { 391 }
    ///     Constant [ <persistent> int T_COMMENT ] { 392 }
    ///     Constant [ <persistent> int T_DOC_COMMENT ] { 393 }
    ///     Constant [ <persistent> int T_OPEN_TAG ] { 394 }
    ///     Constant [ <persistent> int T_OPEN_TAG_WITH_ECHO ] { 395 }
    ///     Constant [ <persistent> int T_CLOSE_TAG ] { 396 }
    ///     Constant [ <persistent> int T_WHITESPACE ] { 397 }
    ///     Constant [ <persistent> int T_START_HEREDOC ] { 398 }
    ///     Constant [ <persistent> int T_END_HEREDOC ] { 399 }
    ///     Constant [ <persistent> int T_DOLLAR_OPEN_CURLY_BRACES ] { 400 }
    ///     Constant [ <persistent> int T_CURLY_OPEN ] { 401 }
    ///     Constant [ <persistent> int T_PAAMAYIM_NEKUDOTAYIM ] { 402 }
    ///     Constant [ <persistent> int T_NS_SEPARATOR ] { 403 }
    ///     Constant [ <persistent> int T_ELLIPSIS ] { 404 }
    ///     Constant [ <persistent> int T_COALESCE ] { 405 }
    ///     Constant [ <persistent> int T_POW ] { 406 }
    ///     Constant [ <persistent> int T_POW_EQUAL ] { 407 }
    ///     Constant [ <persistent> int T_PIPE ] { 408 }
    ///     Constant [ <persistent> int T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG ] { 409 }
    ///     Constant [ <persistent> int T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG ] { 410 }
    ///     Constant [ <persistent> int T_BAD_CHARACTER ] { 411 }
    ///     Constant [ <persistent> int T_DOUBLE_COLON ] { 402 }
    ///     Constant [ <persistent> int TOKEN_PARSE ] { 1 }
    ///   }
    ///
    ///   - Functions {
    ///     Function [ <internal:tokenizer> function token_get_all ] {
    ///
    ///       - Parameters [2] {
    ///         Parameter #0 [ <required> string $code ]
    ///         Parameter #1 [ <optional> int $flags = 0 ]
    ///       }
    ///       - Return [ array ]
    ///     }
    ///     Function [ <internal:tokenizer> function token_name ] {
    ///
    ///       - Parameters [1] {
    ///         Parameter #0 [ <required> int $id ]
    ///       }
    ///       - Return [ string ]
    ///     }
    ///   }
    ///
    ///   - Classes [1] {
    ///     Class [ <internal:tokenizer> class PhpToken implements Stringable ] {
    ///
    ///       - Constants [0] {
    ///       }
    ///
    ///       - Static properties [0] {
    ///       }
    ///
    ///       - Static methods [1] {
    ///         Method [ <internal:tokenizer> static public method tokenize ] {
    ///
    ///           - Parameters [2] {
    ///             Parameter #0 [ <required> string $code ]
    ///             Parameter #1 [ <optional> int $flags = 0 ]
    ///           }
    ///           - Return [ array ]
    ///         }
    ///       }
    ///
    ///       - Properties [4] {
    ///         Property [ public int $id ]
    ///         Property [ public string $text ]
    ///         Property [ public int $line ]
    ///         Property [ public int $pos ]
    ///       }
    ///
    ///       - Methods [5] {
    ///         Method [ <internal:tokenizer, ctor> final public method __construct ] {
    ///
    ///           - Parameters [4] {
    ///             Parameter #0 [ <required> int $id ]
    ///             Parameter #1 [ <required> string $text ]
    ///             Parameter #2 [ <optional> int $line = -1 ]
    ///             Parameter #3 [ <optional> int $pos = -1 ]
    ///           }
    ///         }
    ///
    ///         Method [ <internal:tokenizer> public method is ] {
    ///
    ///           - Parameters [1] {
    ///             Parameter #0 [ <required> $kind ]
    ///           }
    ///           - Return [ bool ]
    ///         }
    ///
    ///         Method [ <internal:tokenizer> public method isIgnorable ] {
    ///
    ///           - Parameters [0] {
    ///           }
    ///           - Return [ bool ]
    ///         }
    ///
    ///         Method [ <internal:tokenizer> public method getTokenName ] {
    ///
    ///           - Parameters [0] {
    ///           }
    ///           - Return [ ?string ]
    ///         }
    ///
    ///         Method [ <internal:tokenizer, prototype Stringable> public method __toString ] {
    ///
    ///           - Parameters [0] {
    ///           }
    ///           - Return [ string ]
    ///         }
    ///       }
    ///     }
    ///   }
    /// }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let text = extension_string(req, extension, "")?;
        PhpString::new(req, text.as_bytes())
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
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let name = req
            .extension_name(extension)
            .map(str::to_owned)
            .unwrap_or_default();
        PhpString::new(req, name.as_bytes())
    }

    /// `ReflectionExtension::getVersion()`: `module->version`, and `null`
    /// for `NO_VERSION_YET`, which no extension compiled in here is: the
    /// version the extension declared, `PHP_VERSION` where it declared
    /// none, which is what `_extension_string`'s header prints.
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('json'))->getVersion());
    /// var_dump((new ReflectionExtension('dom'))->getVersion());
    /// echo preg_replace('/#\d+/', '#N', strstr(new ReflectionExtension('dom'), "\n", true)), "\n";
    /// ```
    /// ```expect
    /// string(5) "8.5.9"
    /// string(8) "20031129"
    /// Extension [ <persistent> extension #N dom version 20031129 ] {
    /// ```
    #[php(tentative)]
    fn getVersion(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let version = req.extension_version(extension).unwrap_or_default();
        PhpString::new(req, version.as_bytes()).map(Some)
    }

    /// `ReflectionExtension::getFunctions()`: one `ReflectionFunction` per
    /// native the extension declared, keyed by
    /// `fptr->common.function_name`, which for a native is the lowered
    /// name php registered it under.
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('json'))->getFunctions());
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["json_encode"]=>
    ///   object(ReflectionFunction)#2 (1) {
    ///     ["name"]=>
    ///     string(11) "json_encode"
    ///   }
    ///   ["json_decode"]=>
    ///   object(ReflectionFunction)#3 (1) {
    ///     ["name"]=>
    ///     string(11) "json_decode"
    ///   }
    ///   ["json_validate"]=>
    ///   object(ReflectionFunction)#4 (1) {
    ///     ["name"]=>
    ///     string(13) "json_validate"
    ///   }
    ///   ["json_last_error"]=>
    ///   object(ReflectionFunction)#5 (1) {
    ///     ["name"]=>
    ///     string(15) "json_last_error"
    ///   }
    ///   ["json_last_error_msg"]=>
    ///   object(ReflectionFunction)#6 (1) {
    ///     ["name"]=>
    ///     string(19) "json_last_error_msg"
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn getFunctions(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let reflector = class_named(req, b"ReflectionFunction")?;
        let mut functions = PhpArray::new(req)?;
        for function in req.extension_functions(extension) {
            let Some(name) = req.function_decl(function).map(|decl| decl.name.0.to_vec()) else {
                continue;
            };
            let object = new_object(req, reflector)?;
            set_name(req, *object, &name)?;
            functions.set_str(req, &name, object.into_value())?;
        }
        Ok(functions)
    }

    /// `ReflectionExtension::getConstants()`: the constants registered
    /// under the extension, keyed by name, each value the constant's, in
    /// the order `EG(zend_constants)` holds them: `Core`'s with `PHP_SAPI`
    /// where `php_module_startup` registers it and what the host
    /// registers after, the CLI's `PHP_CLI_PROCESS_TITLE` and its three
    /// streams, phpdbg's three streams alone. `PHP_BUILD_PROVIDER` is the
    /// build's, declared only where it named a provider, as the oracle's
    /// did, so the fences leave it out:
    ///
    /// ```php
    /// $names = array_keys((new ReflectionExtension('Core'))->getConstants());
    /// echo implode(",", array_diff($names, ['PHP_BUILD_PROVIDER'])), "\n";
    /// ```
    /// ```expect process
    /// E_ERROR,E_WARNING,E_PARSE,E_NOTICE,E_CORE_ERROR,E_CORE_WARNING,E_COMPILE_ERROR,E_COMPILE_WARNING,E_USER_ERROR,E_USER_WARNING,E_USER_NOTICE,E_STRICT,E_RECOVERABLE_ERROR,E_DEPRECATED,E_USER_DEPRECATED,E_ALL,DEBUG_BACKTRACE_PROVIDE_OBJECT,DEBUG_BACKTRACE_IGNORE_ARGS,ZEND_THREAD_SAFE,ZEND_DEBUG_BUILD,ZEND_VM_KIND,TRUE,FALSE,NULL,PHP_VERSION,PHP_MAJOR_VERSION,PHP_MINOR_VERSION,PHP_RELEASE_VERSION,PHP_EXTRA_VERSION,PHP_VERSION_ID,PHP_BUILD_DATE,PHP_ZTS,PHP_DEBUG,PHP_OS,PHP_OS_FAMILY,DEFAULT_INCLUDE_PATH,PEAR_INSTALL_DIR,PEAR_EXTENSION_DIR,PHP_EXTENSION_DIR,PHP_PREFIX,PHP_BINDIR,PHP_SBINDIR,PHP_MANDIR,PHP_LIBDIR,PHP_DATADIR,PHP_SYSCONFDIR,PHP_LOCALSTATEDIR,PHP_CONFIG_FILE_PATH,PHP_CONFIG_FILE_SCAN_DIR,PHP_SHLIB_SUFFIX,PHP_EOL,PHP_MAXPATHLEN,PHP_INT_MAX,PHP_INT_MIN,PHP_INT_SIZE,PHP_FD_SETSIZE,PHP_FLOAT_DIG,PHP_FLOAT_EPSILON,PHP_FLOAT_MAX,PHP_FLOAT_MIN,PHP_OUTPUT_HANDLER_START,PHP_OUTPUT_HANDLER_WRITE,PHP_OUTPUT_HANDLER_FLUSH,PHP_OUTPUT_HANDLER_CLEAN,PHP_OUTPUT_HANDLER_FINAL,PHP_OUTPUT_HANDLER_CONT,PHP_OUTPUT_HANDLER_END,PHP_OUTPUT_HANDLER_CLEANABLE,PHP_OUTPUT_HANDLER_FLUSHABLE,PHP_OUTPUT_HANDLER_REMOVABLE,PHP_OUTPUT_HANDLER_STDFLAGS,PHP_OUTPUT_HANDLER_STARTED,PHP_OUTPUT_HANDLER_DISABLED,PHP_OUTPUT_HANDLER_PROCESSED,UPLOAD_ERR_OK,UPLOAD_ERR_INI_SIZE,UPLOAD_ERR_FORM_SIZE,UPLOAD_ERR_PARTIAL,UPLOAD_ERR_NO_FILE,UPLOAD_ERR_NO_TMP_DIR,UPLOAD_ERR_CANT_WRITE,UPLOAD_ERR_EXTENSION,PHP_SAPI,PHP_BINARY,PHP_CLI_PROCESS_TITLE,STDIN,STDOUT,STDERR
    /// ```
    ///
    /// ```php
    /// $names = array_keys((new ReflectionExtension('Core'))->getConstants());
    /// echo implode(",", array_diff($names, ['PHP_BUILD_PROVIDER'])), "\n";
    /// ```
    /// ```request phpdbg
    /// r
    /// q
    /// ```
    /// ```expect
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> E_ERROR,E_WARNING,E_PARSE,E_NOTICE,E_CORE_ERROR,E_CORE_WARNING,E_COMPILE_ERROR,E_COMPILE_WARNING,E_USER_ERROR,E_USER_WARNING,E_USER_NOTICE,E_STRICT,E_RECOVERABLE_ERROR,E_DEPRECATED,E_USER_DEPRECATED,E_ALL,DEBUG_BACKTRACE_PROVIDE_OBJECT,DEBUG_BACKTRACE_IGNORE_ARGS,ZEND_THREAD_SAFE,ZEND_DEBUG_BUILD,ZEND_VM_KIND,TRUE,FALSE,NULL,PHP_VERSION,PHP_MAJOR_VERSION,PHP_MINOR_VERSION,PHP_RELEASE_VERSION,PHP_EXTRA_VERSION,PHP_VERSION_ID,PHP_BUILD_DATE,PHP_ZTS,PHP_DEBUG,PHP_OS,PHP_OS_FAMILY,DEFAULT_INCLUDE_PATH,PEAR_INSTALL_DIR,PEAR_EXTENSION_DIR,PHP_EXTENSION_DIR,PHP_PREFIX,PHP_BINDIR,PHP_SBINDIR,PHP_MANDIR,PHP_LIBDIR,PHP_DATADIR,PHP_SYSCONFDIR,PHP_LOCALSTATEDIR,PHP_CONFIG_FILE_PATH,PHP_CONFIG_FILE_SCAN_DIR,PHP_SHLIB_SUFFIX,PHP_EOL,PHP_MAXPATHLEN,PHP_INT_MAX,PHP_INT_MIN,PHP_INT_SIZE,PHP_FD_SETSIZE,PHP_FLOAT_DIG,PHP_FLOAT_EPSILON,PHP_FLOAT_MAX,PHP_FLOAT_MIN,PHP_OUTPUT_HANDLER_START,PHP_OUTPUT_HANDLER_WRITE,PHP_OUTPUT_HANDLER_FLUSH,PHP_OUTPUT_HANDLER_CLEAN,PHP_OUTPUT_HANDLER_FINAL,PHP_OUTPUT_HANDLER_CONT,PHP_OUTPUT_HANDLER_END,PHP_OUTPUT_HANDLER_CLEANABLE,PHP_OUTPUT_HANDLER_FLUSHABLE,PHP_OUTPUT_HANDLER_REMOVABLE,PHP_OUTPUT_HANDLER_STDFLAGS,PHP_OUTPUT_HANDLER_STARTED,PHP_OUTPUT_HANDLER_DISABLED,PHP_OUTPUT_HANDLER_PROCESSED,UPLOAD_ERR_OK,UPLOAD_ERR_INI_SIZE,UPLOAD_ERR_FORM_SIZE,UPLOAD_ERR_PARTIAL,UPLOAD_ERR_NO_FILE,UPLOAD_ERR_NO_TMP_DIR,UPLOAD_ERR_CANT_WRITE,UPLOAD_ERR_EXTENSION,PHP_SAPI,PHP_BINARY,STDIN,STDOUT,STDERR
    /// [Script ended normally]
    /// prompt>
    /// ```
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('ctype'))->getConstants());
    /// ```
    /// ```expect
    /// array(0) {
    /// }
    /// ```
    #[php(tentative)]
    fn getConstants(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let mut constants = PhpArray::new(req)?;
        for declared in req.extension_constants(extension) {
            let name = declared.0.to_vec();
            let value = constant(req, &name)?.unwrap_or_default();
            // The lookup is a view and the row is the array's:
            // `ZVAL_COPY_OR_DUP`, so the table keeps its own count.
            constants.set_str(req, &name, Owner::take(req, value))?;
        }
        Ok(constants)
    }

    /// `ReflectionExtension::getINIEntries()`: `_addinientry` over the
    /// extension's entries, each name keyed to the value in force, `null`
    /// where the entry has none (php's `NULL`):
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('openssl'))->getINIEntries()['openssl.cafile']);
    /// ```
    /// ```expect
    /// NULL
    /// ```
    ///
    /// ```php
    /// ini_set('date.default_latitude', '1');
    /// var_dump((new ReflectionExtension('date'))->getINIEntries());
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["date.timezone"]=>
    ///   string(3) "UTC"
    ///   ["date.default_latitude"]=>
    ///   string(1) "1"
    ///   ["date.default_longitude"]=>
    ///   string(7) "35.2333"
    ///   ["date.sunset_zenith"]=>
    ///   string(9) "90.833333"
    ///   ["date.sunrise_zenith"]=>
    ///   string(9) "90.833333"
    /// }
    /// ```
    #[php(tentative)]
    fn getINIEntries(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let mut entries = PhpArray::new(req)?;
        for entry in req.extension_ini(extension) {
            let value = match entry.value {
                Some(value) => PhpString::new(req, &value)?.into_value(),
                None => Owner::null(),
            };
            entries.set_str(req, &entry.decl.name.0, value)?;
        }
        Ok(entries)
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
    #[php(tentative)]
    fn getClasses(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let mut classes = PhpArray::new(req)?;
        for (class, name) in extension_classes(req, extension) {
            let reflector = reflection_class_factory(req, class)?;
            classes.set_str(req, &name, reflector.into_value())?;
        }
        Ok(classes)
    }

    /// `ReflectionExtension::getClassNames()`: the same walk as
    /// `getClasses`, the names appended in order. `Core`'s are in the
    /// order php's `EG(class_table)` holds them, and the iterator wrapper, which
    /// php never adds to it, is not among them:
    ///
    /// ```php
    /// echo implode(",", (new ReflectionExtension('Core'))->getClassNames()), "\n";
    /// ```
    /// ```expect process
    /// Traversable,IteratorAggregate,Iterator,Serializable,ArrayAccess,Countable,Stringable,InternalIterator,Throwable,Exception,ErrorException,Error,CompileError,ParseError,TypeError,ArgumentCountError,ValueError,ArithmeticError,DivisionByZeroError,UnhandledMatchError,RequestParseBodyException,Closure,Generator,ClosedGeneratorException,WeakReference,WeakMap,Attribute,ReturnTypeWillChange,AllowDynamicProperties,SensitiveParameter,SensitiveParameterValue,Override,Deprecated,NoDiscard,DelayedTargetValidation,UnitEnum,BackedEnum,Fiber,FiberError,stdClass
    /// ```
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('json'))->getClassNames());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(16) "JsonSerializable"
    ///   [1]=>
    ///   string(13) "JsonException"
    /// }
    /// ```
    #[php(tentative)]
    fn getClassNames(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let mut names = PhpArray::new(req)?;
        for (_, name) in extension_classes(req, extension) {
            let name = PhpString::new(req, &name)?.into_value();
            names.append(req, name)?;
        }
        Ok(names)
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
    #[php(tentative)]
    fn getDependencies(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let extension = this_extension(req, this.object())?;
        let listed = req.extension_dependencies(extension);
        if listed.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut dependencies = PhpArray::new(req)?;
        for dependency in listed {
            let mut relation = String::from(match dependency.kind {
                DependencyKind::Required => "Required",
                DependencyKind::Conflicts => "Conflicts",
                DependencyKind::Optional => "Optional",
            });
            for part in [&dependency.rel, &dependency.version].into_iter().flatten() {
                relation.push(' ');
                relation.push_str(part);
            }
            let value = PhpString::new(req, relation.as_bytes())?.into_value();
            dependencies.set_str(req, dependency.name.as_bytes(), value)?;
        }
        Ok(dependencies)
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
    #[php(tentative)]
    fn info(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let extension = this_extension(req, this.object())?;
        req.extension_info(extension);
        Ok(())
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
    #[php(tentative)]
    fn isPersistent(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        this_extension(req, this.object())?;
        Ok(true)
    }

    /// `ReflectionExtension::isTemporary()`:
    /// `module->type == MODULE_TEMPORARY`, the complement of
    /// `isPersistent`.
    ///
    /// ```php
    /// var_dump((new ReflectionExtension('json'))->isTemporary());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isTemporary(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        this_extension(req, this.object())?;
        Ok(false)
    }
}

/// `ReflectionZendExtension` as the stub declares it.
#[php_class(implements(Reflector), not_serializable, size = 96)]
pub(crate) struct ReflectionZendExtension;

#[php_class]
impl ReflectionZendExtension {
    #[php(property)]
    const name: PhpString;

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionZendExtension::__construct(string $name)`:
    /// `zend_get_extension(name)`, the Zend extension of that name
    /// exactly, case and all, with `$name` written, or php's
    /// `ReflectionException`.
    ///
    /// ```php
    /// try { new ReflectionZendExtension('zend_opcache'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { new ReflectionZendExtension('zend opcache'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { new ReflectionZendExtension([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->name);
    /// ```
    /// ```expect
    /// Zend Extension "zend_opcache" does not exist
    /// Zend Extension "zend opcache" does not exist
    /// ReflectionZendExtension::__construct(): Argument #1 ($name) must be of type string, array given
    /// string(12) "Zend OPcache"
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, name: PhpString) -> Result<(), Thrown> {
        let name = name.as_bytes().to_vec();
        if zend_extension_named(req, &name).is_none() {
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Zend Extension \"{}\" does not exist",
                    String::from_utf8_lossy(&name)
                ),
            ));
        }
        set_name(req, this.object(), &name)
    }

    /// `ReflectionZendExtension::__toString()`, `_zend_extension_string`:
    /// the name, then the version, the copyright, `by` the author and the
    /// URL in angle brackets, each where the row carries one. Its five
    /// readers each read one field of the row the constructor found; a
    /// reflector no constructor ran is `GET_REFLECTION_OBJECT_PTR`'s
    /// `Error` after the count check.
    ///
    /// ```php
    /// $z = new ReflectionZendExtension('Zend OPcache');
    /// echo $z;
    /// var_dump($z->getName(), $z->getVersion() === PHP_VERSION, $z->getAuthor(), $z->getURL(), $z->getCopyright());
    /// $r = (new ReflectionClass('ReflectionZendExtension'))->newInstanceWithoutConstructor();
    /// foreach (['getName', 'getVersion', 'getAuthor', 'getURL', 'getCopyright', '__toString'] as $m) {
    ///     try { call_user_func([$r, $m]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// try { $r->getName(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Zend Extension [ Zend OPcache %s Copyright (c) by Zend Technologies <http://www.zend.com/> ]
    /// string(12) "Zend OPcache"
    /// bool(true)
    /// string(17) "Zend Technologies"
    /// string(20) "http://www.zend.com/"
    /// string(13) "Copyright (c)"
    /// Internal error: Failed to retrieve the reflection object
    /// Internal error: Failed to retrieve the reflection object
    /// Internal error: Failed to retrieve the reflection object
    /// Internal error: Failed to retrieve the reflection object
    /// Internal error: Failed to retrieve the reflection object
    /// Internal error: Failed to retrieve the reflection object
    /// ReflectionZendExtension::getName() expects exactly 0 arguments, 1 given
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        let mut text = format!("Zend Extension [ {} ", row.name);
        for (field, before, after) in [
            (&row.version, "", ""),
            (&row.copyright, "", ""),
            (&row.author, "by ", ""),
            (&row.url, "<", ">"),
        ] {
            if !field.is_empty() {
                write!(text, "{before}{field}{after} ").ok();
            }
        }
        text.push_str("]\n");
        PhpString::new(req, text.as_bytes())
    }

    /// `ReflectionZendExtension::getName()`: the row's `name`.
    ///
    /// ```php
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->getName());
    /// ```
    /// ```expect
    /// string(12) "Zend OPcache"
    /// ```
    #[php(tentative)]
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        PhpString::new(req, row.name.as_bytes())
    }

    /// `ReflectionZendExtension::getVersion()`: the row's `version`, opcache's being `PHP_VERSION`.
    ///
    /// ```php
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->getVersion() === PHP_VERSION);
    /// ```
    /// ```expect
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn getVersion(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        PhpString::new(req, row.version.as_bytes())
    }

    /// `ReflectionZendExtension::getAuthor()`: the row's `author`.
    ///
    /// ```php
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->getAuthor());
    /// ```
    /// ```expect
    /// string(17) "Zend Technologies"
    /// ```
    #[php(tentative)]
    fn getAuthor(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        PhpString::new(req, row.author.as_bytes())
    }

    /// `ReflectionZendExtension::getURL()`: the row's `URL`.
    ///
    /// ```php
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->getURL());
    /// ```
    /// ```expect
    /// string(20) "http://www.zend.com/"
    /// ```
    #[php(tentative)]
    fn getURL(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        PhpString::new(req, row.url.as_bytes())
    }

    /// `ReflectionZendExtension::getCopyright()`: the row's `copyright`.
    ///
    /// ```php
    /// var_dump((new ReflectionZendExtension('Zend OPcache'))->getCopyright());
    /// ```
    /// ```expect
    /// string(13) "Copyright (c)"
    /// ```
    #[php(tentative)]
    fn getCopyright(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let row = this_zend_extension(req, this.object())?;
        PhpString::new(req, row.copyright.as_bytes())
    }
}

/// `zend_get_extension`: the Zend extension named `name`, compared as
/// `strcmp` compares, case and all.
fn zend_extension_named(req: &Request, name: &[u8]) -> Option<ZendExtension> {
    req.zend_extensions()
        .into_iter()
        .find(|row| row.name.as_bytes() == name)
}

/// `GET_REFLECTION_OBJECT_PTR(extension)` for a `ReflectionZendExtension`:
/// the row the constructor found under `$name`; a reflector whose
/// constructor never ran is php's `Error`.
fn this_zend_extension(req: &mut Request, this: PhpObject) -> Result<ZendExtension, Thrown> {
    let name = this
        .prop(req, b"name")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    match name.and_then(|name| zend_extension_named(req, &name)) {
        Some(row) => Ok(row),
        None => Err(throw_error(
            req,
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

/// `ReflectionReference` as the stub declares it.
#[php_class(final, not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionReference;

#[php_class]
impl ReflectionReference {
    /// `ReflectionReference::fromArrayElement(array $array, int|string
    /// $key)`: a reflector over the element at `key` where it is a
    /// reference, `null` where it is a plain value or a reference only the
    /// array holds (`is_ignorable_reference`, a directly self-referential
    /// array excepted), and php's `ReflectionException` for a key the array
    /// has not. The key is looked up as given, `zend_hash_find` for a
    /// string and `zend_hash_index_find` for an int.
    ///
    /// ```php
    /// $a = [0, 1, 2]; $r = &$a[1]; $one = &$a[2]; unset($one);
    /// var_dump(ReflectionReference::fromArrayElement($a, 0), ReflectionReference::fromArrayElement($a, 1), ReflectionReference::fromArrayElement($a, 2));
    /// try { ReflectionReference::fromArrayElement($a, 3); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// try { ReflectionReference::fromArrayElement($a, []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $s = []; $s[0] = &$s; $t = [$s]; unset($s);
    /// var_dump(ReflectionReference::fromArrayElement($t[0], 0) instanceof ReflectionReference);
    /// ```
    /// ```expect
    /// NULL
    /// object(ReflectionReference)#1 (0) {
    /// }
    /// NULL
    /// Array key not found
    /// ReflectionReference::fromArrayElement(): Argument #2 ($key) must be of type string|int, array given
    /// bool(true)
    /// ```
    fn fromArrayElement(
        req: &mut Request,
        array: PhpArray,
        key: Union<i64, PhpString>,
    ) -> Result<Option<Owner<Instance<ReflectionReference>>>, Thrown> {
        let key = match key {
            Union::A(n) => Key::Int(n),
            Union::B(name) => Key::Str(name),
        };
        let Some(item) = array.get(&key) else {
            return Err(throw(req, "ReflectionException", "Array key not found"));
        };
        let Some(reference) = item.as_php_ref() else {
            return Ok(None);
        };
        // `is_ignorable_reference`: one owner, the array's, and no directly
        // self-referential array, which `zend_array_dup` keeps as a
        // reference despite the one owner.
        let self_referential = reference
            .get()
            .as_array()
            .and_then(|held| held.get(&key))
            .and_then(|element| element.as_php_ref())
            == Some(reference);
        if item.refcount() == 1 && !self_referential {
            return Ok(None);
        }
        let class = class_named(req, b"ReflectionReference")?;
        let object = new_object(req, class)?;
        let held = Owner::take(req, reference);
        match object.native_mut::<Reflected>(req) {
            Some(reflected) => reflected.reference = Some(held),
            None => held.give_up(req),
        }
        Ok(Some(object.into_instance()))
    }

    /// `ReflectionReference::getId()`: a string that is the same for two
    /// reflectors over one reference and differs between two references,
    /// twenty bytes as php's `SHA1(ref || key)` is, here the reference's
    /// identity hashed under the request's secret, so no address is shown;
    /// php's `Corrupted ReflectionReference object` for a reflector that
    /// holds none.
    ///
    /// ```php
    /// $a = [1, 2]; $x = &$a[0]; $y = &$a[1]; $b = []; $b[0] = &$a[0];
    /// $r0 = ReflectionReference::fromArrayElement($a, 0); $r1 = ReflectionReference::fromArrayElement($a, 1); $rb = ReflectionReference::fromArrayElement($b, 0);
    /// var_dump($r0->getId() === $rb->getId(), $r0->getId() !== $r1->getId(), strlen($r0->getId()));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// int(20)
    /// ```
    fn getId(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        use std::hash::BuildHasher as _;
        let reference = this
            .object()
            .native::<Reflected>(req)
            .and_then(|reflected| reflected.reference.as_deref().copied());
        let Some(reference) = reference else {
            return Err(throw(
                req,
                "ReflectionException",
                "Corrupted ReflectionReference object",
            ));
        };
        let key = &req.state::<State>().reference_key;
        let digest: Vec<u8> = (0u8..3)
            .flat_map(|round| key.hash_one((reference, round)).to_le_bytes())
            .take(20)
            .collect();
        PhpString::new(req, &digest)
    }

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionReference::__construct()`: private, and php's refusal
    /// when reached anyway, since a reflector is made by
    /// `fromArrayElement`.
    ///
    /// ```php
    /// try { new ReflectionReference(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Call to private ReflectionReference::__construct() from global scope
    /// ```
    ///
    /// ```php
    /// $r = ReflectionReference::fromArrayElement([&$x], 0);
    /// try { (new ReflectionMethod('ReflectionReference', '__construct'))->invoke($r); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot directly instantiate ReflectionReference. Use ReflectionReference::fromArrayElement() instead
    /// ```
    #[php(private, arity = manual)]
    fn __construct(req: &mut Request, _: This<Self>) -> Result<(), Thrown> {
        Err(throw(
            req,
            "ReflectionException",
            "Cannot directly instantiate ReflectionReference. Use ReflectionReference::fromArrayElement() instead",
        ))
    }
}

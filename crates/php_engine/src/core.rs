//! Zend's own natives, classes and constants as one value (design §7.1):
//! what `zend_startup_builtin_functions` registers under the name `Core`,
//! each native, class and constant declared by the attributes where it
//! is written and listed here once, in the order php registers it, which
//! is the order `get_defined_functions()`, `get_declared_classes()` and
//! `get_defined_constants()` list them in.

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::attributes::{
    AllowDynamicProperties, Attribute, DelayedTargetValidation, Deprecated, NoDiscard, Override,
    ReturnTypeWillChange, SensitiveParameter, SensitiveParameterValue,
};
use crate::classes::{
    ArgumentCountError, ArithmeticError, ArrayAccess, BackedEnum, Closure, CompileError,
    DivisionByZeroError, Error, ErrorException, Exception, ParseError, RequestParseBodyException,
    Stringable, Throwable, TypeError, UnhandledMatchError, UnitEnum, ValueError, stdClass,
};
use crate::fiber::{Fiber, FiberError};
use crate::generator::{ClosedGeneratorException, Generator};
use crate::interfaces::{
    Countable, InternalIterator, Iterator, IteratorAggregate, Serializable, Traversable,
};
use crate::weakrefs::{WeakMap, WeakReference};

/// The `Core` value: the functions of `zend_builtin_functions.stub.php`
/// ([`crate::natives::core`]); the classes in the order php's
/// `EG(class_table)` holds them, `zend_register_default_classes`'s
/// (`Z/Zend/zend_default_classes.c`): the interfaces, the exceptions, the
/// iterator wrapper `zend_register_iterator_wrapper` makes, `Closure`,
/// the generators, the weak references, the attributes, the enums' two
/// interfaces and the fibers, then `stdClass`, which
/// `ZEND_MINIT_FUNCTION(core)` registers after them
/// (`Z/Zend/zend_builtin_functions.c:40`); and the constants
/// ([`crate::constants::startup`]).
pub(crate) fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .functions(crate::natives::core())
        .class::<Traversable>()
        .class::<IteratorAggregate>()
        .class::<Iterator>()
        .class::<Serializable>()
        .class::<ArrayAccess>()
        .class::<Countable>()
        .class::<Stringable>()
        .class::<InternalIterator>()
        .class::<Throwable>()
        .class::<Exception>()
        .class::<ErrorException>()
        .class::<Error>()
        .class::<CompileError>()
        .class::<ParseError>()
        .class::<TypeError>()
        .class::<ArgumentCountError>()
        .class::<ValueError>()
        .class::<ArithmeticError>()
        .class::<DivisionByZeroError>()
        .class::<UnhandledMatchError>()
        .class::<RequestParseBodyException>();
    crate::interfaces::iterator_wrapper(builder)
        .class::<Closure>()
        .class::<Generator>()
        .class::<ClosedGeneratorException>()
        .class::<WeakReference>()
        .class::<WeakMap>()
        .class::<Attribute>()
        .class::<ReturnTypeWillChange>()
        .class::<AllowDynamicProperties>()
        .class::<SensitiveParameter>()
        .class::<SensitiveParameterValue>()
        .class::<Override>()
        .class::<Deprecated>()
        .class::<NoDiscard>()
        .class::<DelayedTargetValidation>()
        .class::<UnitEnum>()
        .class::<BackedEnum>()
        .class::<Fiber>()
        .class::<FiberError>()
        .class::<stdClass>()
        .constants(crate::constants::startup::declared())
        .build("Core")
}

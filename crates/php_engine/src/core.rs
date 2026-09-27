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
    DivisionByZeroError, Error, Exception, ParseError, RequestParseBodyException, Stringable,
    Throwable, TypeError, UnhandledMatchError, UnitEnum, ValueError, stdClass,
};
use crate::fiber::{Fiber, FiberError};
use crate::generator::{ClosedGeneratorException, Generator};
use crate::interfaces::{
    Countable, InternalIterator, Iterator, IteratorAggregate, Serializable, Traversable,
};
use crate::weakrefs::{WeakMap, WeakReference};

/// The `Core` value: the functions of `zend_builtin_functions.stub.php`
/// ([`crate::natives::core`]); the classes as `zend_register_interfaces`,
/// `zend_register_default_classes` and `zend_register_fiber_ce` register
/// them, the iterator wrapper among the interfaces' as
/// `zend_register_iterator_wrapper` makes it; and the constants
/// ([`crate::constants::startup`]).
pub(crate) fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .functions(crate::natives::core())
        .class::<Traversable>()
        .class::<IteratorAggregate>()
        .class::<Iterator>()
        .class::<Serializable>()
        .class::<Countable>()
        .class::<InternalIterator>();
    crate::interfaces::iterator_wrapper(builder)
        .class::<Stringable>()
        .class::<Throwable>()
        .class::<ArrayAccess>()
        .class::<Exception>()
        .class::<Error>()
        .class::<TypeError>()
        .class::<ValueError>()
        .class::<ArithmeticError>()
        .class::<DivisionByZeroError>()
        .class::<ArgumentCountError>()
        .class::<UnhandledMatchError>()
        .class::<CompileError>()
        .class::<ParseError>()
        .class::<RequestParseBodyException>()
        .class::<Closure>()
        .class::<Generator>()
        .class::<ClosedGeneratorException>()
        .class::<stdClass>()
        .class::<UnitEnum>()
        .class::<BackedEnum>()
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
        .class::<Fiber>()
        .class::<FiberError>()
        .constants(crate::constants::startup::declared())
        .build("Core")
}

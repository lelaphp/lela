//! `spl_exceptions.stub.php`: `LogicException` and `RuntimeException`
//! under `Exception`, and their eleven children, as
//! `PHP_MINIT_FUNCTION(spl_exceptions)` registers them, in its order.

use php_ext::abi::ExtensionBuilder;
use php_ext::attr::php_class;

use crate::Exception;

/// The thirteen classes onto `builder`, in php's order.
///
/// ```php
/// try { throw new OutOfBoundsException("past the end", 3); }
/// catch (RuntimeException $e) { echo get_class($e), ": ", $e->getMessage(), " ", $e->getCode(), "\n"; }
/// var_dump(new BadMethodCallException instanceof LogicException, get_parent_class("UnderflowException"));
/// throw new LengthException("too long");
/// ```
/// ```expect exit=255
/// OutOfBoundsException: past the end 3
/// bool(true)
/// string(16) "RuntimeException"
///
/// Fatal error: Uncaught LengthException: too long in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 5
/// ```
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<LogicException>()
        .class::<BadFunctionCallException>()
        .class::<BadMethodCallException>()
        .class::<DomainException>()
        .class::<InvalidArgumentException>()
        .class::<LengthException>()
        .class::<OutOfRangeException>()
        .class::<RuntimeException>()
        .class::<OutOfBoundsException>()
        .class::<OverflowException>()
        .class::<RangeException>()
        .class::<UnderflowException>()
        .class::<UnexpectedValueException>()
}

/// `class LogicException extends Exception`, the root of the errors in
/// a program's logic.
#[php_class(extends(Exception))]
pub(crate) struct LogicException;

#[php_class]
impl LogicException {}

#[php_class(extends(LogicException))]
pub(crate) struct BadFunctionCallException;

#[php_class]
impl BadFunctionCallException {}

#[php_class(extends(BadFunctionCallException))]
pub(crate) struct BadMethodCallException;

#[php_class]
impl BadMethodCallException {}

#[php_class(extends(LogicException))]
pub(crate) struct DomainException;

#[php_class]
impl DomainException {}

#[php_class(extends(LogicException))]
pub(crate) struct InvalidArgumentException;

#[php_class]
impl InvalidArgumentException {}

#[php_class(extends(LogicException))]
pub(crate) struct LengthException;

#[php_class]
impl LengthException {}

#[php_class(extends(LogicException))]
pub(crate) struct OutOfRangeException;

#[php_class]
impl OutOfRangeException {}

/// `class RuntimeException extends Exception`, the root of the errors
/// no reading of the program would have found.
#[php_class(extends(Exception))]
pub(crate) struct RuntimeException;

#[php_class]
impl RuntimeException {}

#[php_class(extends(RuntimeException))]
pub(crate) struct OutOfBoundsException;

#[php_class]
impl OutOfBoundsException {}

#[php_class(extends(RuntimeException))]
pub(crate) struct OverflowException;

#[php_class]
impl OverflowException {}

#[php_class(extends(RuntimeException))]
pub(crate) struct RangeException;

#[php_class]
impl RangeException {}

#[php_class(extends(RuntimeException))]
pub(crate) struct UnderflowException;

#[php_class]
impl UnderflowException {}

#[php_class(extends(RuntimeException))]
pub(crate) struct UnexpectedValueException;

#[php_class]
impl UnexpectedValueException {}

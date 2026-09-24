//! `spl_exceptions.stub.php`: `LogicException` and `RuntimeException`
//! under `Exception`, and their eleven children, as
//! `PHP_MINIT_FUNCTION(spl_exceptions)` registers them, in its order.

use crate::stub::{self, Declared};

const STUB: &str = "
class LogicException extends Exception {}
class BadFunctionCallException extends LogicException {}
class BadMethodCallException extends BadFunctionCallException {}
class DomainException extends LogicException {}
class InvalidArgumentException extends LogicException {}
class LengthException extends LogicException {}
class OutOfRangeException extends LogicException {}
class RuntimeException extends Exception {}
class OutOfBoundsException extends RuntimeException {}
class OverflowException extends RuntimeException {}
class RangeException extends RuntimeException {}
class UnderflowException extends RuntimeException {}
class UnexpectedValueException extends RuntimeException {}
";

/// The thirteen classes.
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
pub(crate) fn classes() -> Vec<Declared> {
    stub::classes(STUB, &[])
}

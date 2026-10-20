//! The natives of `zend_builtin_functions.c` over the two handler
//! stacks php keeps per request, `EG(user_error_handler)` and
//! `EG(user_exception_handler)` with the stacks under them, and the two
//! that raise a diagnostic of the program's own, `trigger_error` and
//! its second name `user_error`. The stacks are the request's
//! ([`Diagnostics::set_error_handler`]); what hands a diagnostic to the
//! handler is `zend_error_impl`'s branch in [`Request::note_error_in`],
//! and what hands it an uncaught throwable is [`Request::uncaught`].

use php_macros::php_function;
use php_unit::Level;

use crate::api::abi::{Callable, FromArgument, Raw, True, Untyped};
use crate::api::raising::{Thrown, arg_value_error, deprecated, none_pending};
use crate::api::request::Request;
use crate::api::values::{PhpString, Value};

/// `trigger_error(string $message, int $error_level = E_USER_NOTICE):
/// true`, php's `ZEND_FUNCTION(trigger_error)`
/// (`Z/Zend/zend_builtin_functions.c:1272`), and `user_error`, the same
/// entry under its second name: the message raised at one of the four
/// user levels where the request stands, through the user handler
/// where one takes it. Any other level is php's `ValueError`.
///
/// ```php
/// var_dump(trigger_error("n"), user_error("w", E_USER_WARNING), trigger_error("d", E_USER_DEPRECATED));
/// trigger_error("x", E_WARNING);
/// ```
/// ```expect exit=255
/// Notice: n in %s on line 2
///
/// Warning: w in %s on line 2
///
/// Deprecated: d in %s on line 2
/// bool(true)
/// bool(true)
/// bool(true)
///
/// Fatal error: Uncaught ValueError: trigger_error(): Argument #2 ($error_level) must be one of E_USER_ERROR, E_USER_WARNING, E_USER_NOTICE, or E_USER_DEPRECATED in %s:3
/// Stack trace:
/// #0 %s(3): trigger_error('x', 2)
/// #1 {main}
///   thrown in %s on line 3
/// ```
///
/// `E_USER_ERROR` is the 8.4 deprecation first, and then, where the
/// built-in handler serves it, the end `php_error_cb` makes of a fatal: the request
/// ends in it, the shutdown functions are served and no destructor is.
///
/// ```php
/// class A { function __destruct() { echo "d\n"; } }
/// $a = new A; register_shutdown_function(function () { echo "shutdown\n"; });
/// trigger_error("boom", E_USER_ERROR); echo "after\n";
/// ```
/// ```expect exit=255
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 4
///
/// Fatal error: boom in %s on line 4
/// shutdown
/// ```
#[php_function(aliases(user_error))]
fn trigger_error(
    req: &mut Request,
    message: PhpString,
    #[php(default = E_USER_NOTICE)] error_level: i64,
) -> Result<True, Thrown> {
    let level = match error_level {
        256 => {
            deprecated(
                req,
                "Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead",
            );
            none_pending(req)?;
            Level::UserError
        }
        512 => Level::UserWarning,
        1024 => Level::UserNotice,
        16384 => Level::UserDeprecated,
        _ => {
            return Err(arg_value_error(
                req,
                2,
                "be one of E_USER_ERROR, E_USER_WARNING, E_USER_NOTICE, or E_USER_DEPRECATED",
            ));
        }
    };
    let served = req.note_error_string(level, message);
    if served && level == Level::UserError {
        req.bail_out();
        return Err(Thrown::of(None));
    }
    Ok(True)
}

/// `set_error_handler(?callable $callback, int $error_levels = E_ALL)`,
/// php's `ZEND_FUNCTION(set_error_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1306`): the handler standing is
/// pushed with its mask and answered, and the callback stands with
/// `$error_levels`. The handler is handed the level, the message, the
/// file and the line of every diagnostic its mask takes, whatever
/// `error_reporting` says, which it reads for itself; an answer of
/// `false` is the fall-through to the built-in handler, and every other
/// answer serves the diagnostic.
///
/// ```php
/// function h($no, $str, $file, $line) { echo "h: $no $str ", $file === __FILE__ ? "here" : $file, ":$line\n"; return $no != E_USER_WARNING; }
/// var_dump(set_error_handler("h"));
/// trigger_error("one");
/// trigger_error("two", E_USER_WARNING);
/// var_dump(set_error_handler(null), get_error_handler());
/// trigger_error("three");
/// ```
/// ```expect
/// NULL
/// h: 1024 one here:4
/// h: 512 two here:5
///
/// Warning: two in %s on line 5
/// string(1) "h"
/// NULL
///
/// Notice: three in %s on line 7
/// ```
///
/// The mask is `$error_levels`; a level outside it reaches the built-in
/// handler. Under `@` the handler is still handed the diagnostic, with
/// `error_reporting()` reading the level the `@` masked, and a
/// diagnostic the engine raises reaches it as one the program raised.
///
/// ```php
/// function h($no, $str) { echo "h: $str ", error_reporting(), "\n"; }
/// set_error_handler("h", E_USER_NOTICE);
/// trigger_error("a", E_USER_WARNING);
/// trigger_error("b");
/// @trigger_error("c");
/// echo $undefined;
/// set_error_handler("h");
/// echo $undefined, "|\n";
/// ```
/// ```expect
/// Warning: a in %s on line 4
/// h: b 30719
/// h: c 4437
///
/// Warning: Undefined variable $undefined in %s on line 7
/// h: Undefined variable $undefined 30719
/// |
/// ```
///
/// What the handler throws takes the diagnostic's place, pending where
/// it was raised.
///
/// ```php
/// set_error_handler(function ($no, $str) { throw new Exception($str); });
/// try { trigger_error("x"); echo "not here\n"; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// try { echo $u; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// caught x
/// caught Undefined variable $u
/// ```
///
/// ```php
/// set_error_handler("nope");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: set_error_handler(): Argument #1 ($callback) must be a valid callback or null, function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): set_error_handler('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn set_error_handler(
    req: &mut Request,
    callback: Option<Callable>,
    #[php(default = E_ALL)] error_levels: i64,
) -> Untyped<Value> {
    let previous = req
        .diagnostics_mut()
        .set_error_handler(callback, error_levels);
    Untyped(previous.unwrap_or_default())
}

/// `restore_error_handler(): true`, php's
/// `ZEND_FUNCTION(restore_error_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1332`): the handler under the one
/// standing stands again, with its mask, and `true` whatever was there.
///
/// ```php
/// set_error_handler("strlen"); set_error_handler("trim"); var_dump(get_error_handler());
/// var_dump(restore_error_handler(), get_error_handler(), restore_error_handler(), get_error_handler(), restore_error_handler(), get_error_handler());
/// ```
/// ```expect
/// string(4) "trim"
/// bool(true)
/// string(6) "strlen"
/// bool(true)
/// NULL
/// bool(true)
/// NULL
/// ```
#[php_function]
fn restore_error_handler(req: &mut Request) -> True {
    req.restore_error_handler();
    True
}

/// `get_error_handler(): ?callable`, php 8.5's
/// `ZEND_FUNCTION(get_error_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1358`): the value the handler
/// standing was set from, and `null` for none. Its witnesses stand
/// beside [`set_error_handler`] and [`restore_error_handler`].
///
/// ```php
/// var_dump(get_error_handler()); set_error_handler(fn () => true); var_dump(get_error_handler() instanceof Closure);
/// ```
/// ```expect
/// NULL
/// bool(true)
/// ```
#[php_function]
fn get_error_handler(req: &mut Request) -> Result<Raw<Option<Callable>>, Thrown> {
    let handler = req.error_handler();
    Raw::from_value(req, handler.unwrap_or_default())
}

/// `set_exception_handler(?callable $callback)`, php's
/// `ZEND_FUNCTION(set_exception_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1367`): the handler standing is
/// pushed and answered, and the callback stands. A throwable nothing
/// caught is handed to it once the main op array is left, and the
/// request goes on to its shutdown as though nothing were thrown; the
/// handler is taken down while it is served, so what it throws is the
/// uncaught fatal, its frame the boundary no opline called.
///
/// ```php
/// function h($e) { echo "h: ", get_class($e), " ", $e->getMessage(), "\n"; var_dump(get_exception_handler()); }
/// var_dump(set_exception_handler("h"), get_exception_handler());
/// echo "before\n";
/// throw new Exception("x");
/// echo "not here\n";
/// ```
/// ```expect
/// NULL
/// string(1) "h"
/// before
/// h: Exception x
/// NULL
/// ```
///
/// ```php
/// set_exception_handler(function ($e) { echo "h\n"; throw new Error("inner"); });
/// throw new Exception("x");
/// ```
/// ```expect exit=255
/// h
///
/// Fatal error: Uncaught Error: inner in %s:2
/// Stack trace:
/// #0 [internal function]: {closure:%s:2}(Object(Exception))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A shutdown function's throwable is handed to it as well, since no
/// frame stands under it.
///
/// ```php
/// set_exception_handler(function ($e) { echo "h ", $e->getMessage(), "\n"; });
/// register_shutdown_function(function () { throw new Exception("late"); });
/// echo "end\n";
/// ```
/// ```expect
/// end
/// h late
/// ```
#[php_function]
fn set_exception_handler(req: &mut Request, callback: Option<Callable>) -> Untyped<Value> {
    let previous = req.set_exception_handler(callback);
    Untyped(previous.unwrap_or_default())
}

/// `restore_exception_handler(): true`, php's
/// `ZEND_FUNCTION(restore_exception_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1399`).
///
/// ```php
/// set_exception_handler("strlen"); set_exception_handler("trim"); var_dump(set_exception_handler(null));
/// var_dump(get_exception_handler(), restore_exception_handler(), get_exception_handler(), restore_exception_handler(), get_exception_handler(), restore_exception_handler(), get_exception_handler());
/// ```
/// ```expect
/// string(4) "trim"
/// NULL
/// bool(true)
/// string(4) "trim"
/// bool(true)
/// string(6) "strlen"
/// bool(true)
/// NULL
/// ```
#[php_function]
fn restore_exception_handler(req: &mut Request) -> True {
    req.restore_exception_handler();
    True
}

/// `get_exception_handler(): ?callable`, php 8.5's
/// `ZEND_FUNCTION(get_exception_handler)`
/// (`Z/Zend/zend_builtin_functions.c:1420`).
///
/// ```php
/// var_dump(get_exception_handler()); set_exception_handler("strlen"); var_dump(get_exception_handler());
/// ```
/// ```expect
/// NULL
/// string(6) "strlen"
/// ```
#[php_function]
fn get_exception_handler(req: &mut Request) -> Result<Raw<Option<Callable>>, Thrown> {
    let handler = req.exception_handler();
    Raw::from_value(req, handler.unwrap_or_default())
}

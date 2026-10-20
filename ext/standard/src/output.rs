//! The `ob_*` natives, `basic_functions.stub.php:1499-1533`, each the
//! `PHP_FUNCTION` of `Z/main/output.c` behind one
//! [`OutputOp`](php_ext::request::OutputOp) on the request's handler
//! stack; the stack, its notices and its memory are the engine's.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::callable_checked;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{OutputOp, Request, StartHandler};
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

/// What an op that answers `true` or `false` answered.
fn op_bool(req: &mut Request, op: &OutputOp) -> Result<bool, Thrown> {
    let answer = req.output_buffer_op(op)?;
    let done = answer.as_bool().unwrap_or(false);
    answer.give_up(req);
    Ok(done)
}

/// What an op that answers the buffer or `false` answered, the string's
/// owner handed on.
fn op_string(req: &mut Request, op: &OutputOp) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let answer = req.output_buffer_op(op)?;
    let string = match answer.as_ref() {
        ValueRef::Str(string) => OrFalse::Value(Owner::take(req, string)),
        _ => OrFalse::False,
    };
    answer.give_up(req);
    Ok(string)
}

/// What an op that answers an array answered, its owner handed on.
fn op_array(req: &mut Request, op: &OutputOp) -> Result<Owner<PhpArray>, Thrown> {
    let answer = req.output_buffer_op(op)?;
    let array = match answer.as_array() {
        Some(array) => Ok(Owner::take(req, array)),
        None => PhpArray::new(req),
    };
    answer.give_up(req);
    array
}

/// `ob_start($callback = null, int $chunk_size = 0, int $flags = PHP_OUTPUT_HANDLER_STDFLAGS): bool`
/// (`Z/main/output.c:1373`): a handler pushed, the default one for no callback, a chunk size
/// below zero read as none. The buffer is the request heap's: the
/// default handler holds its eighty-byte record, its name, a 16 KiB
/// buffer and the stack's first block of sixteen slots, and a chunk
/// size rounds the buffer up to 4 KiB. Ending it gives all but the
/// stack's block back, which `zend_stack` keeps until the request
/// ends.
///
/// ```php
/// $b = memory_get_usage(); ob_start(); $held = memory_get_usage() - $b;
/// ob_end_clean(); $after = memory_get_usage() - $b;
/// ob_start(null, 100); $chunked = memory_get_usage() - $b;
/// ob_end_clean(); var_dump($held, $after, $chunked);
/// ```
/// ```expect
/// int(16640)
/// int(128)
/// int(4352)
/// ```
///
/// A handler of its own holds the 128-byte user record beside all of
/// that, and holds the callback's own string as its name rather than a
/// copy, where the default handler's name is forty-eight bytes of its
/// own: eighty more than the default handler, not a hundred and
/// seventy-six.
///
/// ```php
/// $b = memory_get_usage(); ob_start('strtoupper'); $held = memory_get_usage() - $b;
/// try { ob_end_clean(); } catch (ArgumentCountError $e) { }
/// var_dump($held);
/// ```
/// ```expect
/// int(16720)
/// ```
///
/// A callback that names no function is php's warning and the notice
/// that no buffer was made.
///
/// ```php
/// var_dump(ob_start("no_such_function"), ob_get_level());
/// ```
/// ```expect
/// Warning: ob_start(): function "no_such_function" not found or invalid function name in %s on line 2
///
/// Notice: ob_start(): Failed to create buffer in %s on line 2
/// bool(false)
/// int(0)
/// ```
///
/// Every other shape reads as `zend_is_callable_ex` words it: a value
/// that is neither an array nor a string, including an object of a
/// class with no `__invoke`; an array of the wrong size; an array whose
/// class half names nothing; and one whose method half does.
///
/// ```php
/// var_dump(ob_start(1));
/// var_dump(ob_start(new stdClass));
/// var_dump(ob_start([1, 2, 3]));
/// var_dump(ob_start(['NoClass', 'method']));
/// var_dump(ob_start([new stdClass, 'method']));
/// var_dump(ob_get_level());
/// ```
/// ```expect
/// Warning: ob_start(): no array or string given in %s on line 2
///
/// Notice: ob_start(): Failed to create buffer in %s on line 2
/// bool(false)
///
/// Warning: ob_start(): no array or string given in %s on line 3
///
/// Notice: ob_start(): Failed to create buffer in %s on line 3
/// bool(false)
///
/// Warning: ob_start(): array callback must have exactly two members in %s on line 4
///
/// Notice: ob_start(): Failed to create buffer in %s on line 4
/// bool(false)
///
/// Warning: ob_start(): class "NoClass" not found in %s on line 5
///
/// Notice: ob_start(): Failed to create buffer in %s on line 5
/// bool(false)
///
/// Warning: ob_start(): class stdClass does not have a method "method" in %s on line 6
///
/// Notice: ob_start(): Failed to create buffer in %s on line 6
/// bool(false)
/// int(0)
/// ```
///
/// A closure is a shape it takes, and the handler runs as any other.
///
/// ```php
/// var_dump(ob_start(function ($buffer, $phase) { return strtoupper($buffer); }));
/// echo "hi"; ob_end_flush(); echo "|\n"; var_dump(ob_get_level());
/// ```
/// ```expect
/// BOOL(TRUE)
/// HI|
/// int(0)
/// ```
///
/// ```php
/// var_dump(ob_get_level(), ob_start(), ob_get_level(), ob_end_flush(), ob_get_level());
/// ```
/// ```expect
/// int(0)
/// bool(true)
/// int(1)
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn ob_start(
    req: &mut Request,
    #[php(default = null)] callback: Untyped<Value>,
    #[php(default = 0)] chunk_size: i64,
    #[php(default = PHP_OUTPUT_HANDLER_STDFLAGS)] flags: i64,
) -> Result<bool, Thrown> {
    let callback = callback.0;
    let registered = match callback.deref().as_ref() {
        ValueRef::Str(name) if !name.is_empty() => req.buffer_handler(name.as_bytes()),
        _ => None,
    };
    let handler = match (callback.deref().as_ref(), registered) {
        (ValueRef::Undef | ValueRef::Null, _) => StartHandler::Default,
        (_, Some(registered)) => StartHandler::Registered(registered),
        _ => match callable_checked(req, &callback) {
            Ok(callable) => StartHandler::User(callable),
            // `zend_fcall_info_init`'s error, which is the sentence
            // `zend_is_callable_ex` words for the shape it refused.
            Err(error) => {
                docref(req, Level::Warning, &error);
                StartHandler::Refused
            }
        },
    };
    op_bool(
        req,
        &OutputOp::Start {
            handler,
            chunk_size: chunk_size.max(0),
            flags,
        },
    )
}

/// `ob_flush(): bool` (`Z/main/output.c:1395`): the buffer sent on
/// through the handlers under it, the handler kept.
///
/// ```php
/// ob_start(); echo "a"; ob_start(); echo "b"; $flushed = ob_flush(); $inner = ob_get_contents(); ob_end_clean(); $outer = ob_get_contents(); ob_end_clean(); var_dump($flushed, $inner, $outer);
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// string(2) "ab"
/// ```
#[php_function]
fn ob_flush(req: &mut Request) -> Result<bool, Thrown> {
    op_bool(req, &OutputOp::Flush)
}

/// `ob_clean(): bool` (`Z/main/output.c:1415`): the buffer emptied,
/// the handler kept; nothing to clean is the notice.
///
/// ```php
/// ob_start(); echo "gone"; $cleaned = ob_clean(); $left = ob_get_contents(); ob_end_flush(); var_dump($cleaned, $left, ob_clean());
/// ```
/// ```expect
/// Notice: ob_clean(): Failed to delete buffer. No buffer to delete in %s on line 2
/// bool(true)
/// string(0) ""
/// bool(false)
/// ```
#[php_function]
fn ob_clean(req: &mut Request) -> Result<bool, Thrown> {
    op_bool(req, &OutputOp::Clean)
}

/// `ob_end_flush(): bool` (`Z/main/output.c:1437`): the buffer sent on
/// and the handler popped.
///
/// ```php
/// ob_start(); echo "kept"; var_dump(ob_end_flush(), ob_get_level()); var_dump(ob_end_flush());
/// ```
/// ```expect
/// keptbool(true)
/// int(0)
///
/// Notice: ob_end_flush(): Failed to delete and flush buffer. No buffer to delete or flush in %s on line 2
/// bool(false)
/// ```
#[php_function]
fn ob_end_flush(req: &mut Request) -> Result<bool, Thrown> {
    op_bool(req, &OutputOp::EndFlush)
}

/// `ob_end_clean(): bool` (`Z/main/output.c:1452`): the buffer
/// dropped and the handler popped; a handler started without
/// `PHP_OUTPUT_HANDLER_REMOVABLE` stays, with the notice.
///
/// ```php
/// ob_start(null, 0, 48); var_dump(ob_end_clean(), ob_get_level());
/// ```
/// ```expect
/// Notice: ob_end_clean(): Failed to discard buffer of default output handler (0) in %s on line 2
/// bool(false)
/// int(1)
/// ```
#[php_function]
fn ob_end_clean(req: &mut Request) -> Result<bool, Thrown> {
    op_bool(req, &OutputOp::EndClean)
}

/// `ob_get_flush(): string|false` (`Z/main/output.c:1467`): the
/// buffer as a string, sent on, and the handler popped.
///
/// ```php
/// ob_start(); echo "twice"; $s = ob_get_flush(); var_dump($s); var_dump(ob_get_flush());
/// ```
/// ```expect
/// twicestring(5) "twice"
///
/// Notice: ob_get_flush(): Failed to delete and flush buffer. No buffer to delete or flush in %s on line 2
/// bool(false)
/// ```
#[php_function]
fn ob_get_flush(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    op_string(req, &OutputOp::GetFlush)
}

/// `ob_get_clean(): string|false` (`Z/main/output.c:1485`): the
/// buffer as a string, dropped, and the handler popped; `false` and
/// no notice with no handler.
///
/// ```php
/// ob_start(); echo "taken"; var_dump(ob_get_clean(), ob_get_clean());
/// ```
/// ```expect
/// string(5) "taken"
/// bool(false)
/// ```
#[php_function]
fn ob_get_clean(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    op_string(req, &OutputOp::GetClean)
}

/// `ob_get_contents(): string|false` (`Z/main/output.c:1508`).
///
/// ```php
/// var_dump(ob_get_contents()); ob_start(); echo "so far"; $s = ob_get_contents(); ob_end_clean(); var_dump($s);
/// ```
/// ```expect
/// bool(false)
/// string(6) "so far"
/// ```
#[php_function]
fn ob_get_contents(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    op_string(req, &OutputOp::GetContents)
}

/// `ob_get_level(): int` (`Z/main/output.c:1520`).
///
/// ```php
/// var_dump(ob_get_level()); ob_start(); ob_start(); $n = ob_get_level(); ob_end_clean(); ob_end_clean(); var_dump($n);
/// ```
/// ```expect
/// int(0)
/// int(2)
/// ```
#[php_function]
fn ob_get_level(req: &mut Request) -> Result<i64, Thrown> {
    let level = req.output_buffer_op(&OutputOp::GetLevel)?;
    let n = level.as_int().unwrap_or(0);
    level.give_up(req);
    Ok(n)
}

/// `ob_get_length(): int|false` (`Z/main/output.c:1531`).
///
/// ```php
/// var_dump(ob_get_length()); ob_start(); echo "four"; $n = ob_get_length(); ob_end_clean(); var_dump($n);
/// ```
/// ```expect
/// bool(false)
/// int(4)
/// ```
#[php_function]
fn ob_get_length(req: &mut Request) -> Result<OrFalse<i64>, Thrown> {
    let length = req.output_buffer_op(&OutputOp::GetLength)?;
    let n = length.as_int();
    length.give_up(req);
    Ok(match n {
        Some(length) => OrFalse::Value(length),
        None => OrFalse::False,
    })
}

/// `ob_list_handlers(): array` (`Z/main/output.c:1544`): the names,
/// bottom first.
///
/// ```php
/// ob_start(); ob_start(); $two = ob_list_handlers(); ob_end_clean(); ob_end_clean(); var_dump($two, ob_list_handlers());
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(22) "default output handler"
///   [1]=>
///   string(22) "default output handler"
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn ob_list_handlers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    op_array(req, &OutputOp::ListHandlers)
}

/// `ob_get_status(bool $full_status = false): array`
/// (`Z/main/output.c:1561`): the active handler's seven keys, or every
/// handler's, bottom first; the flags read `STARTED` and `PROCESSED`
/// once the handler has been served.
///
/// ```php
/// ob_start(null, 100); echo "abc"; $one = ob_get_status(); ob_clean(); $all = ob_get_status(true); ob_end_clean(); var_dump($one, $all, ob_get_status());
/// ```
/// ```expect
/// array(7) {
///   ["name"]=>
///   string(22) "default output handler"
///   ["type"]=>
///   int(0)
///   ["flags"]=>
///   int(112)
///   ["level"]=>
///   int(0)
///   ["chunk_size"]=>
///   int(100)
///   ["buffer_size"]=>
///   int(4096)
///   ["buffer_used"]=>
///   int(3)
/// }
/// array(1) {
///   [0]=>
///   array(7) {
///     ["name"]=>
///     string(22) "default output handler"
///     ["type"]=>
///     int(0)
///     ["flags"]=>
///     int(20592)
///     ["level"]=>
///     int(0)
///     ["chunk_size"]=>
///     int(100)
///     ["buffer_size"]=>
///     int(4096)
///     ["buffer_used"]=>
///     int(0)
///   }
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn ob_get_status(
    req: &mut Request,
    #[php(default = false)] full_status: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    op_array(req, &OutputOp::GetStatus { full: full_status })
}

/// `ob_implicit_flush(bool $enable = true): void`
/// (`Z/main/output.c:1580`): `sapi_flush` after every write to the
/// host, which under a host handed the body once is nothing to see.
///
/// ```php
/// var_dump(ob_implicit_flush(), ob_implicit_flush(false));
/// ```
/// ```expect
/// NULL
/// NULL
/// ```
#[php_function]
fn ob_implicit_flush(req: &mut Request, #[php(default = true)] enable: bool) -> Result<(), Thrown> {
    let done = req.output_buffer_op(&OutputOp::ImplicitFlush(enable))?;
    done.give_up(req);
    Ok(())
}

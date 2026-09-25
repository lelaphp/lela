//! The `ob_*` natives, `basic_functions.stub.php:1499-1533`, each the
//! `PHP_FUNCTION` of `Z/main/output.c` behind one
//! [`OutputOp`](php_ext::request::OutputOp) on the request's handler
//! stack; the stack, its notices and its memory are the engine's.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::{callable_checked, coerce_param};
use php_ext::decl::{ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, arg_count_error, docref};
use php_ext::request::{OutputOp, Request, StartHandler};
use php_ext::values::{Value, ValueRef};

use crate::{declared, parameter};

/// A native's entry, as the builder takes it.
pub(crate) type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The thirteen, in the stub's order.
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
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let string_or_false = || Type::Union(vec![Type::String, Type::False]);
    vec![
        (
            declared(
                b"ob_start",
                &[
                    Parameter {
                        ty: None,
                        default: ParameterDefault::Expr(ConstExpr::Null),
                        ..parameter(b"callback", Type::Mixed)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Long(0)),
                        ..parameter(b"chunk_size", Type::Int)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Constant(Name(
                            b"PHP_OUTPUT_HANDLER_STDFLAGS".as_slice().into(),
                        ))),
                        ..parameter(b"flags", Type::Int)
                    },
                ],
                Type::Bool,
            ),
            ob_start as Entry,
        ),
        (declared(b"ob_flush", &[], Type::Bool), ob_flush as Entry),
        (declared(b"ob_clean", &[], Type::Bool), ob_clean as Entry),
        (
            declared(b"ob_end_flush", &[], Type::Bool),
            ob_end_flush as Entry,
        ),
        (
            declared(b"ob_end_clean", &[], Type::Bool),
            ob_end_clean as Entry,
        ),
        (
            declared(b"ob_get_flush", &[], string_or_false()),
            ob_get_flush as Entry,
        ),
        (
            declared(b"ob_get_clean", &[], string_or_false()),
            ob_get_clean as Entry,
        ),
        (
            declared(b"ob_get_contents", &[], string_or_false()),
            ob_get_contents as Entry,
        ),
        (
            declared(b"ob_get_level", &[], Type::Int),
            ob_get_level as Entry,
        ),
        (
            declared(
                b"ob_get_length",
                &[],
                Type::Union(vec![Type::Int, Type::False]),
            ),
            ob_get_length as Entry,
        ),
        (
            declared(b"ob_list_handlers", &[], Type::Array),
            ob_list_handlers as Entry,
        ),
        (
            declared(
                b"ob_get_status",
                &[Parameter {
                    default: ParameterDefault::Expr(ConstExpr::Bool(false)),
                    ..parameter(b"full_status", Type::Bool)
                }],
                Type::Array,
            ),
            ob_get_status as Entry,
        ),
        (
            declared(
                b"ob_implicit_flush",
                &[Parameter {
                    default: ParameterDefault::Expr(ConstExpr::Bool(true)),
                    ..parameter(b"enable", Type::Bool)
                }],
                Type::Void,
            ),
            ob_implicit_flush as Entry,
        ),
    ]
}

/// `zend_parse_parameters_none`.
fn no_arguments(req: &mut Request) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given == 0 {
        Ok(())
    } else {
        Err(arg_count_error(
            req,
            &format!("exactly 0 arguments, {given} given"),
        ))
    }
}

/// `zend_parse_parameters` for a native of optional parameters alone:
/// more than `most` is the count error.
fn at_most(req: &mut Request, most: usize) -> Result<usize, Thrown> {
    let given = req.argument_count();
    if given > most {
        let word = if most == 1 { "argument" } else { "arguments" };
        return Err(arg_count_error(
            req,
            &format!("at most {most} {word}, {given} given"),
        ));
    }
    Ok(given)
}

/// `Z_PARAM_LONG` at `position`, coerced as php coerces an int.
fn long_at(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// `Z_PARAM_BOOL` at `position`.
fn bool_at(req: &mut Request, position: u32) -> Result<bool, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Bool, position)?;
    Ok(coerced.as_bool().unwrap_or(false))
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
fn ob_start(req: &mut Request) -> Result<Value, Thrown> {
    let given = at_most(req, 3)?;
    let callback = req.argument(1).unwrap_or_default();
    let chunk_size = if given >= 2 { long_at(req, 2)? } else { 0 };
    let flags = if given >= 3 { long_at(req, 3)? } else { 0x70 };
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
                docref(req, Level::Notice, "Failed to create buffer");
                return Ok(Value::bool(false));
            }
        },
    };
    req.output_buffer_op(&OutputOp::Start {
        handler,
        chunk_size: chunk_size.max(0),
        flags,
    })
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
fn ob_flush(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::Flush)
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
fn ob_clean(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::Clean)
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
fn ob_end_flush(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::EndFlush)
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
fn ob_end_clean(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::EndClean)
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
fn ob_get_flush(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::GetFlush)
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
fn ob_get_clean(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::GetClean)
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
fn ob_get_contents(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::GetContents)
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
fn ob_get_level(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::GetLevel)
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
fn ob_get_length(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::GetLength)
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
fn ob_list_handlers(req: &mut Request) -> Result<Value, Thrown> {
    no_arguments(req)?;
    req.output_buffer_op(&OutputOp::ListHandlers)
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
fn ob_get_status(req: &mut Request) -> Result<Value, Thrown> {
    let given = at_most(req, 1)?;
    let full = given == 1 && bool_at(req, 1)?;
    req.output_buffer_op(&OutputOp::GetStatus { full })
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
fn ob_implicit_flush(req: &mut Request) -> Result<Value, Thrown> {
    let given = at_most(req, 1)?;
    let enable = if given == 1 { bool_at(req, 1)? } else { true };
    req.output_buffer_op(&OutputOp::ImplicitFlush(enable))
}

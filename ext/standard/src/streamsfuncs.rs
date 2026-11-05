//! `Z/ext/standard/streamsfuncs.c`, the natives over the streams facet
//! that need no socket and no filter chain: the stream `contexts`, the
//! stream layer's `stream-context` resources (`StreamCtx`, as #1777
//! ruled), with their options and their notification callback;
//! `stream_get_contents`, `stream_copy_to_stream`
//! and `stream_get_line` over the stream's reads; the options
//! `stream_set_*` asks of a stream, answered as the kinds this engine has
//! answer them; and `stream_resolve_include_path`, `stream_is_local`,
//! `stream_supports_lock` and `stream_get_wrappers`. `stream_get_meta_data`
//! is `file`'s, which declares `socket_get_status` as its alias, listed
//! here in the stub's order. The socket
//! natives, `stream_select`, the filter natives, `stream_isatty` and
//! `stream_get_transports` wait on faces the api does not have (#684).
//!
//! The natives, as the stub declares them.
//!
//! ```php
//! foreach (['stream_context_create', 'stream_context_set_option', 'stream_get_contents', 'stream_copy_to_stream', 'stream_get_line', 'stream_set_blocking', 'socket_get_status'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function stream_context_create ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <optional> ?array $options = null ]
//!     Parameter #1 [ <optional> ?array $params = null ]
//!   }
//! }
//! Function [ <internal:standard> function stream_context_set_option ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $context ]
//!     Parameter #1 [ <required> array|string $wrapper_or_options ]
//!     Parameter #2 [ <optional> ?string $option_name = null ]
//!     Parameter #3 [ <optional> mixed $value = <default> ]
//!   }
//!   - Return [ true ]
//! }
//! Function [ <internal:standard> function stream_get_contents ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <optional> ?int $length = null ]
//!     Parameter #2 [ <optional> int $offset = -1 ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function stream_copy_to_stream ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $from ]
//!     Parameter #1 [ <required> $to ]
//!     Parameter #2 [ <optional> ?int $length = null ]
//!     Parameter #3 [ <optional> int $offset = 0 ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function stream_get_line ] {
//!
//!   - Parameters [3] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> int $length ]
//!     Parameter #2 [ <optional> string $ending = "" ]
//!   }
//!   - Return [ string|false ]
//! }
//! Function [ <internal:standard> function stream_set_blocking ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> bool $enable ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function socket_get_status ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream ]
//!   }
//!   - Return [ array ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;

use crate::throwables::Throwables;
use php_ext::abi::{OrFalse, Passed, Path, Raw, True, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{callable_checked, with_string};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, arity, deprecated, docref,
    type_error, value_error,
};
use php_ext::request::{
    MAXPATHLEN, Notification, NotifyCode, OptionAnswer, Request, Resolve, Stream, StreamCtx,
    StreamOption, Streams as _, Unresolved, Whence,
};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpString, Resource, Value, ValueRef};

/// `sizeof(zend_fcall_info_cache)`, the callable's cache
/// `parse_context_params` `emalloc`s for the notifier's `ptr`: the
/// function, the calling and called scopes, the object and the closure.
const FCC_SIZE: usize = 40;

/// `PHP_STREAM_COPY_ALL` as `stream_get_contents` spells it, `-1`.
const COPY_ALL: i64 = -1;

/// What `php_stream_context_from_zval`'s `nocontext` says a `null`
/// `$context` is: `FG(default_context)`, or, under
/// `FILE_NO_DEFAULT_CONTEXT` and where php asks only for one given,
/// nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NullContext {
    Default,
    None,
}

/// `php_stream_context_from_zval(zcontext, nocontext)`
/// (`Z/main/streams/php_stream_context.h:36`) over the `$context` a
/// native's declaration held as `Z_PARAM_RESOURCE_OR_NULL`: the one given,
/// which must be a `stream-context` ([`StreamCtx::fetch`]), or for
/// `null` `FG(default_context)` ([`StreamCtx::default_ctx`]),
/// unless `nocontext` says none. The first open of a request that is
/// handed no `$context` makes it, so under the CLI it is resource 4,
/// after the three standard streams, and the stream is 5. What it answers
/// is the `context` the native's stream operations are handed
/// ([`Request::with_stream_ctx`]).
pub(crate) fn context_of(
    req: &mut Request,
    given: Option<Resource>,
    nocontext: NullContext,
) -> Result<Option<StreamCtx>, Thrown> {
    match (given, nocontext) {
        (Some(res), _) => StreamCtx::fetch(req, res).map(Some),
        (None, NullContext::Default) => StreamCtx::default_ctx(req).map(Some),
        (None, NullContext::None) => Ok(None),
    }
}

/// One `php_stream_notify_*` told the notifier of the `context` an
/// operation was handed, where it was handed one.
pub(crate) fn notify(
    req: &mut Request,
    context: Option<StreamCtx>,
    notification: Notification<'_>,
) -> Result<(), Thrown> {
    context.map_or(Ok(()), |context| context.notify(req, notification))
}

/// `php_stream_notify_info(context, code, message, message_code)`.
pub(crate) fn notify_info(
    req: &mut Request,
    context: Option<StreamCtx>,
    code: NotifyCode,
    message: Option<&[u8]>,
    message_code: i64,
) -> Result<(), Thrown> {
    let info = Notification::Info {
        code,
        message,
        message_code,
    };
    notify(req, context, info)
}

/// `php_stream_notify_error(context, code, message, message_code)`.
pub(crate) fn notify_error(
    req: &mut Request,
    context: Option<StreamCtx>,
    code: NotifyCode,
    message: Option<&[u8]>,
    message_code: i64,
) -> Result<(), Thrown> {
    let error = Notification::Error {
        code,
        message,
        message_code,
    };
    notify(req, context, error)
}

/// `Z_PARAM_RESOURCE` then `decode_context_param`
/// (`Z/ext/standard/streamsfuncs.c:937`): a `context`, or a stream's,
/// `PHP_STREAM_CONTEXT(stream)`, a new one made and kept by a stream that
/// keeps none; anything else is php's `must be a valid stream/context`.
fn context_resource(req: &mut Request, res: Resource, position: u32) -> Result<StreamCtx, Thrown> {
    if let Some(context) = StreamCtx::from_resource(req, res) {
        return Ok(context);
    }
    if let Some(stream) = Stream::from_value(req, &res.as_value()) {
        if let Some(context) = stream.ctx(req) {
            return Ok(context);
        }
        let context = StreamCtx::alloc(req)?;
        let view = *context;
        stream.set_ctx(req, Some(view));
        // The stream holds the owner `php_stream_context_alloc` made;
        // `stream->ctx = context->res` takes it without one more.
        context.give_up(req);
        return Ok(view);
    }
    Err(arg_type_error(req, position, "be a valid stream/context"))
}

/// `parse_context_options` (`Z/ext/standard/streamsfuncs.c:872`): each
/// string key holding an array sets its string-keyed options, in order;
/// the first entry that is neither is php's `ValueError`, what came
/// before it already set.
fn parse_options(req: &mut Request, res: StreamCtx, options: PhpArray) -> Result<(), Thrown> {
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(options, wkey, wval)` and
    // `ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(Z_ARRVAL_P(wval), okey, oval)`
    // (`Z/ext/standard/streamsfuncs.c:877`, `:881`), no owner of either
    // table taken: the argument holds them while an option is set.
    walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
        let wval = row.value().deref();
        match (row.key(), wval.as_array()) {
            (Key::Str(wrapper), Some(inner)) => {
                walk::foreach(req, inner, Hold::None, |req, row| -> walk::Step {
                    if let Key::Str(option) = row.key() {
                        let value = row.value().deref();
                        res.set_option(req, wrapper.as_bytes(), option.as_bytes(), &value)?;
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
                Ok(ControlFlow::Continue(()))
            }
            _ => Err(value_error(
                req,
                "Options should have the form [\"wrappername\"][\"optionname\"] = $value",
            )),
        }
    })?;
    Ok(())
}

/// `parse_context_params` (`Z/ext/standard/streamsfuncs.c:896`):
/// `notification` a callable, php's `TypeError` otherwise, which
/// replaces the one there; `options` an array of options, php's
/// `Invalid stream/context parameter` otherwise.
fn parse_params(req: &mut Request, res: StreamCtx, params: PhpArray) -> Result<(), Thrown> {
    if let Some(callback) = params.find(b"notification") {
        // The notifier there is freed first, so a callable refused below
        // leaves the context with none.
        res.set_notifier(req, None)?;
        let fcc = Emalloc::new(req, FCC_SIZE)?;
        if let Err(why) = callable_checked(req, &callback) {
            fcc.free(req);
            return Err(arg_type_error(
                req,
                1,
                &format!("be an array with valid callbacks as values, {why}"),
            ));
        }
        let callback = Owner::take(req, callback);
        res.set_notifier(req, Some((callback, fcc)))?;
    }
    if let Some(options) = params.find(b"options") {
        let options = options.deref();
        let Some(options) = options.as_array() else {
            return Err(type_error(req, "Invalid stream/context parameter"));
        };
        return parse_options(req, res, options);
    }
    Ok(())
}

/// `stream_context_create(?array $options = null, ?array $params = null)`,
/// `PHP_FUNCTION(stream_context_create)`
/// (`Z/ext/standard/streamsfuncs.c:1177`): a new `context`, a resource of
/// the `stream-context` type, with the options and the params set.
///
/// ```php
/// $c = stream_context_create(['http' => ['method' => 'POST', 'timeout' => 5], 'ssl' => [0 => 'skipped', 'verify_peer' => false]], ['notification' => 'strlen']);
/// var_dump($c, stream_context_get_options($c), stream_context_get_params($c));
/// try { stream_context_create(['http' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_create([], ['notification' => 'nope']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_create([], ['options' => 1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// resource(%d) of type (stream-context)
/// array(2) {
///   ["http"]=>
///   array(2) {
///     ["method"]=>
///     string(4) "POST"
///     ["timeout"]=>
///     int(5)
///   }
///   ["ssl"]=>
///   array(1) {
///     ["verify_peer"]=>
///     bool(false)
///   }
/// }
/// array(2) {
///   ["notification"]=>
///   string(6) "strlen"
///   ["options"]=>
///   array(2) {
///     ["http"]=>
///     array(2) {
///       ["method"]=>
///       string(4) "POST"
///       ["timeout"]=>
///       int(5)
///     }
///     ["ssl"]=>
///     array(1) {
///       ["verify_peer"]=>
///       bool(false)
///     }
///   }
/// }
/// Options should have the form ["wrappername"]["optionname"] = $value
/// stream_context_create(): Argument #1 ($options) must be an array with valid callbacks as values, function "nope" not found or invalid function name
/// Invalid stream/context parameter
/// ```
#[php_function]
fn stream_context_create(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpArray>,
    #[php(default = null)] params: Option<PhpArray>,
) -> Result<Untyped<Owner<Resource>>, Thrown> {
    let res = StreamCtx::alloc(req)?;
    let mut applied = Ok(());
    if let Some(options) = options {
        applied = parse_options(req, *res, options);
    }
    if let (Ok(()), Some(params)) = (&applied, params) {
        applied = parse_params(req, *res, params);
    }
    match applied {
        Ok(()) => Ok(Untyped(res.into_resource())),
        Err(thrown) => {
            res.give_up(req);
            Err(thrown)
        }
    }
}

/// `stream_context_set_params($context, array $params): true`,
/// `PHP_FUNCTION(stream_context_set_params)`
/// (`Z/ext/standard/streamsfuncs.c:1074`).
///
/// ```php
/// $c = stream_context_create();
/// var_dump(stream_context_set_params($c, ['notification' => 'trim', 'options' => ['file' => ['x' => 1]]]), stream_context_get_params($c));
/// try { stream_context_set_params('x', []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_set_params(opendir(__DIR__), []); closedir(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// array(2) {
///   ["notification"]=>
///   string(4) "trim"
///   ["options"]=>
///   array(1) {
///     ["file"]=>
///     array(1) {
///       ["x"]=>
///       int(1)
///     }
///   }
/// }
/// stream_context_set_params(): Argument #1 ($context) must be of type resource, string given
///
/// Deprecated: closedir(): Passing null is deprecated, instead the last opened directory stream should be provided in %s on line 5
/// ```
#[php_function]
fn stream_context_set_params(
    req: &mut Request,
    context: Resource,
    params: PhpArray,
) -> Result<True, Thrown> {
    let res = context_resource(req, context, 1)?;
    parse_params(req, res, params)?;
    Ok(True)
}

/// `stream_context_get_params($context): array`,
/// `PHP_FUNCTION(stream_context_get_params)`
/// (`Z/ext/standard/streamsfuncs.c:1100`): `notification` where one is
/// set, then `options`, the `context`'s own array with one more owner.
///
/// ```php
/// var_dump(stream_context_get_params(stream_context_create(['a' => ['b' => 'c']])));
/// ```
/// ```expect
/// array(1) {
///   ["options"]=>
///   array(1) {
///     ["a"]=>
///     array(1) {
///       ["b"]=>
///       string(1) "c"
///     }
///   }
/// }
/// ```
#[php_function]
fn stream_context_get_params(
    req: &mut Request,
    context: Resource,
) -> Result<Owner<PhpArray>, Thrown> {
    let res = context_resource(req, context, 1)?;
    let (options, notification) = (res.options(req), res.notifier(req));
    let mut array = PhpArray::new(req)?;
    if let Some(notification) = notification {
        array.set_str(req, b"notification", Owner::take(req, notification))?;
    }
    let options = match options {
        Some(options) => Owner::take(req, Value::array(options)),
        None => PhpArray::new(req)?.into_value(),
    };
    array.set_str(req, b"options", options)?;
    Ok(array)
}

/// `stream_context_set_option($context, array|string $wrapper_or_options, ?string $option_name = null, mixed $value = UNKNOWN): true`,
/// `PHP_FUNCTION(stream_context_set_option)`
/// (`Z/ext/standard/streamsfuncs.c:985`): the 8.4 deprecation for the
/// two-argument call, then either an array of options, with neither of
/// the last two given, or a wrapper's name with the option's and its
/// value.
///
/// ```php
/// $c = stream_context_create();
/// var_dump(stream_context_set_option($c, 'http', 'method', 'GET'), stream_context_set_option($c, ['ftp' => ['overwrite' => true]]), stream_context_get_options($c));
/// try { stream_context_set_option($c, [], 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_set_option($c, [], null, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_set_option($c, 'http'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_context_set_option($c, 'http', 'a'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Calling stream_context_set_option() with 2 arguments is deprecated, use stream_context_set_options() instead in %s on line 3
/// bool(true)
/// bool(true)
/// array(2) {
///   ["http"]=>
///   array(1) {
///     ["method"]=>
///     string(3) "GET"
///   }
///   ["ftp"]=>
///   array(1) {
///     ["overwrite"]=>
///     bool(true)
///   }
/// }
/// stream_context_set_option(): Argument #3 ($option_name) must be null when argument #2 ($wrapper_or_options) is an array
/// stream_context_set_option(): Argument #4 ($value) cannot be provided when argument #2 ($wrapper_or_options) is an array
///
/// Deprecated: Calling stream_context_set_option() with 2 arguments is deprecated, use stream_context_set_options() instead in %s on line 6
/// stream_context_set_option(): Argument #3 ($option_name) cannot be null when argument #2 ($wrapper_or_options) is a string
/// stream_context_set_option(): Argument #4 ($value) must be provided when argument #2 ($wrapper_or_options) is a string
/// ```
#[php_function(arity = manual)]
fn stream_context_set_option(
    req: &mut Request,
    context: Raw<Resource>,
    wrapper_or_options: Raw<Union<PhpArray, PhpString>>,
    #[php(default = null)] option_name: Raw<Option<PhpString>>,
    value: Passed<Value>,
) -> Result<True, Thrown> {
    // The 8.4 deprecation comes before `ZEND_PARSE_PARAMETERS_START(2, 4)`.
    let given = req.frames().argument_count();
    if given == 2 {
        deprecated(
            req,
            "Calling stream_context_set_option() with 2 arguments is deprecated, use stream_context_set_options() instead",
        );
    }
    arity(req, 2, Some(4))?;
    let context = context.hold(req)?;
    let res = context_resource(req, context, 1)?;
    let wrapper_or_options = wrapper_or_options.hold(req)?;
    let option = option_name.hold(req)?;
    let value = match value {
        Passed::Given(value) => Some(value),
        Passed::Omitted => None,
    };
    let refuse = |req: &mut Request, position: u32, tail: &str| {
        let ce = Throwables::of(req)?;
        Err(argument_error(req, ce.value_error, position, tail))
    };
    match wrapper_or_options {
        Union::A(options) => {
            if option.is_some() {
                return refuse(
                    req,
                    3,
                    "must be null when argument #2 ($wrapper_or_options) is an array",
                );
            }
            if value.is_some() {
                return refuse(
                    req,
                    4,
                    "cannot be provided when argument #2 ($wrapper_or_options) is an array",
                );
            }
            parse_options(req, res, options)?;
        }
        Union::B(wrapper) => {
            let Some(option) = option else {
                return refuse(
                    req,
                    3,
                    "cannot be null when argument #2 ($wrapper_or_options) is a string",
                );
            };
            let Some(value) = value else {
                return refuse(
                    req,
                    4,
                    "must be provided when argument #2 ($wrapper_or_options) is a string",
                );
            };
            res.set_option(req, wrapper.as_bytes(), option.as_bytes(), &value)?;
        }
    }
    Ok(True)
}

/// `stream_context_set_options($context, array $options): true`,
/// `PHP_FUNCTION(stream_context_set_options)`
/// (`Z/ext/standard/streamsfuncs.c:1049`).
///
/// ```php
/// $c = stream_context_create(['a' => ['x' => 1]]);
/// var_dump(stream_context_set_options($c, ['a' => ['y' => 2], 'b' => ['z' => 3]]), stream_context_get_options($c));
/// ```
/// ```expect
/// bool(true)
/// array(2) {
///   ["a"]=>
///   array(2) {
///     ["x"]=>
///     int(1)
///     ["y"]=>
///     int(2)
///   }
///   ["b"]=>
///   array(1) {
///     ["z"]=>
///     int(3)
///   }
/// }
/// ```
#[php_function]
fn stream_context_set_options(
    req: &mut Request,
    context: Resource,
    options: PhpArray,
) -> Result<True, Thrown> {
    let res = context_resource(req, context, 1)?;
    parse_options(req, res, options)?;
    Ok(True)
}

/// `stream_context_get_options($stream_or_context): array`,
/// `PHP_FUNCTION(stream_context_get_options)`
/// (`Z/ext/standard/streamsfuncs.c:965`): the `context`'s options array,
/// one more owner of it, which a later option separates from.
///
/// ```php
/// $c = stream_context_create(['a' => ['b' => 1]]); $o = stream_context_get_options($c);
/// stream_context_set_option($c, 'a', 'c', 2);
/// var_dump($o, count(stream_context_get_options($c)['a']), stream_context_get_options(fopen('php://memory', 'r')));
/// ```
/// ```expect
/// array(1) {
///   ["a"]=>
///   array(1) {
///     ["b"]=>
///     int(1)
///   }
/// }
/// int(2)
/// array(0) {
/// }
/// ```
#[php_function]
fn stream_context_get_options(
    req: &mut Request,
    stream_or_context: Resource,
) -> Result<Owner<PhpArray>, Thrown> {
    let res = context_resource(req, stream_or_context, 1)?;
    let options = res.options(req);
    match options {
        Some(options) => Ok(Owner::take(req, options)),
        None => PhpArray::new(req),
    }
}

/// `stream_context_get_default(?array $options = null)`,
/// `PHP_FUNCTION(stream_context_get_default)`
/// (`Z/ext/standard/streamsfuncs.c:1128`): the request's default
/// `context`, made on first use, with the options set, and one more owner
/// of it handed back.
///
/// ```php
/// $d = stream_context_get_default(['x' => ['y' => 1]]);
/// var_dump($d === stream_context_get_default(), stream_context_get_options(stream_context_set_default(['x' => ['z' => 2]])));
/// ```
/// ```expect
/// bool(true)
/// array(1) {
///   ["x"]=>
///   array(2) {
///     ["y"]=>
///     int(1)
///     ["z"]=>
///     int(2)
///   }
/// }
/// ```
#[php_function]
fn stream_context_get_default(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<Untyped<Resource>, Thrown> {
    let res = StreamCtx::default_ctx(req)?;
    if let Some(options) = options {
        parse_options(req, res, options)?;
    }
    Ok(Untyped(res.resource()))
}

/// `stream_context_set_default(array $options)`,
/// `PHP_FUNCTION(stream_context_set_default)`
/// (`Z/ext/standard/streamsfuncs.c:1154`): the default `context`'s
/// options set, and the `context` handed back.
///
/// ```php
/// var_dump(stream_context_set_default(['q' => ['r' => 's']]));
/// try { stream_context_set_default([1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// resource(%d) of type (stream-context)
/// Options should have the form ["wrappername"]["optionname"] = $value
/// ```
#[php_function]
fn stream_context_set_default(
    req: &mut Request,
    options: PhpArray,
) -> Result<Untyped<Resource>, Thrown> {
    let res = StreamCtx::default_ctx(req)?;
    parse_options(req, res, options)?;
    Ok(Untyped(res.resource()))
}

/// `stream_get_contents($stream, ?int $length = null, int $offset = -1): string|false`,
/// `PHP_FUNCTION(stream_get_contents)`
/// (`Z/ext/standard/streamsfuncs.c:432`): a length below `-1` is php's
/// `ValueError`; an offset at or past zero is sought to first, forward
/// from where the stream stands and from its start otherwise, with php's
/// warning where the seek fails; then the rest of the stream, or up to
/// `$length` bytes of it.
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "hello world");
/// var_dump(stream_get_contents($m), stream_get_contents($m, -1, 0), stream_get_contents($m, 3, 6), stream_get_contents($m), stream_get_contents($m, 0, 1));
/// try { stream_get_contents($m, -2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(stream_get_contents(fopen('php://output', 'w'), -1, 5));
/// ```
/// ```expect
/// string(0) ""
/// string(11) "hello world"
/// string(3) "wor"
/// string(2) "ld"
/// string(0) ""
/// stream_get_contents(): Argument #2 ($length) must be greater than or equal to -1
///
/// Warning: stream_get_contents(): Failed to seek to position 5 in the stream in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn stream_get_contents(
    req: &mut Request,
    stream: Stream,
    #[php(default = null)] length: Option<i64>,
    #[php(default = -1)] offset: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let maxlen = length.unwrap_or(COPY_ALL);
    let desiredpos = offset;
    if maxlen < 0 && maxlen != COPY_ALL {
        return Err(arg_value_error(req, 2, "be greater than or equal to -1"));
    }
    if desiredpos >= 0 {
        let position = stream.tell(req);
        let sought = if position >= 0 && desiredpos > position {
            stream.seek(req, desiredpos - position, Whence::Cur)
        } else if desiredpos < position {
            stream.seek(req, desiredpos, Whence::Set)
        } else {
            true
        };
        if !sought {
            let text = format!("Failed to seek to position {desiredpos} in the stream");
            docref(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    }
    let maxlen = (maxlen != COPY_ALL).then(|| usize::try_from(maxlen).unwrap_or(0));
    if maxlen == Some(0) {
        return Ok(OrFalse::Value(PhpString::new(req, b"")?));
    }
    Ok(OrFalse::Value(stream.copy_to_mem(req, maxlen)?))
}

/// `stream_copy_to_stream($from, $to, ?int $length = null, int $offset = 0): int|false`,
/// `PHP_FUNCTION(stream_copy_to_stream)` over
/// `_php_stream_copy_to_stream_ex` (`Z/main/streams/streams.c:1661`):
/// a positive offset sought to first, with php's warning where it
/// fails; then chunks of 8192 bytes, or what is left of `$length`, read
/// and written until a read gives nothing, and the bytes written; a read
/// or a write that fails is `false`.
///
/// ```php
/// $a = fopen('php://memory', 'r+'); fwrite($a, "0123456789"); rewind($a);
/// $b = fopen('php://memory', 'r+');
/// var_dump(stream_copy_to_stream($a, $b), stream_copy_to_stream($a, $b, null, 2), stream_copy_to_stream($a, $b, 3, 1), stream_copy_to_stream($a, $b, 0), stream_get_contents($b, -1, 0));
/// var_dump(stream_copy_to_stream($a, fopen('php://output', 'w'), null, 8), stream_copy_to_stream(fopen('php://output', 'w'), $b, null, 1));
/// ```
/// ```expect
/// int(10)
/// int(8)
/// int(3)
/// int(0)
/// string(21) "012345678923456789123"
/// 89
/// Warning: stream_copy_to_stream(): Stream does not support seeking in %s on line 5
///
/// Warning: stream_copy_to_stream(): Failed to seek to position 1 in the stream in %s on line 5
/// int(2)
/// bool(false)
/// ```
#[php_function]
fn stream_copy_to_stream(
    req: &mut Request,
    from: Stream,
    to: Stream,
    #[php(default = null)] length: Option<i64>,
    #[php(default = 0)] offset: i64,
) -> OrFalse<i64> {
    let (src, dest, maxlen, pos) = (from, to, length, offset);
    if pos > 0 && !src.seek(req, pos, Whence::Set) {
        let text = format!("Failed to seek to position {pos} in the stream");
        docref(req, Level::Warning, &text);
        return OrFalse::False;
    }
    // `PHP_STREAM_COPY_ALL`, and any other length the `size_t` makes
    // larger than a stream holds, is no limit.
    let maxlen = match maxlen {
        None => None,
        Some(0) => return OrFalse::Value(0),
        Some(n) => usize::try_from(n).ok(),
    };
    match copy_stream(req, src, dest, maxlen) {
        Some(haveread) => OrFalse::Value(i64::try_from(haveread).unwrap_or(i64::MAX)),
        None => OrFalse::False,
    }
}

/// `_php_stream_copy_to_stream_ex` (`Z/main/streams/streams.c:1661`)
/// from where `src` stands: a chunk read into php's `char buf[CHUNK_SIZE]`
/// and written until a read gives nothing or `maxlen` bytes are had,
/// `None` for no limit. The count copied, or `None` for a read or a write
/// that failed, which `stream_copy_to_stream` answers `false` and `copy`
/// answers `false` for. The chunk is the copy's own buffer and no string,
/// so a copy needs no memory of the request's whatever the stream holds
/// (GH-13071):
///
/// ```php
/// $f = __DIR__ . "/big"; file_put_contents($f, str_repeat("a", 4 << 20));
/// ini_set("memory_limit", memory_get_usage(true) + (2 << 20)); var_dump(copy($f, "$f.2"), filesize("$f.2"));
/// ```
/// ```expect
/// bool(true)
/// int(4194304)
/// ```
pub(crate) fn copy_stream(
    req: &mut Request,
    src: Stream,
    dest: Stream,
    maxlen: Option<usize>,
) -> Option<usize> {
    let mut buf = [0u8; 8192];
    let mut haveread: usize = 0;
    loop {
        let readchunk = maxlen.map_or(buf.len(), |max| (max - haveread).min(buf.len()));
        let didread = src.read_into(req, &mut buf[..readchunk])?;
        if didread == 0 {
            break;
        }
        let mut written = 0;
        while written < didread {
            match dest.write(req, &buf[written..didread]) {
                Some(n) if n > 0 => written += n,
                _ => return None,
            }
        }
        haveread += didread;
        if maxlen.is_some_and(|max| haveread >= max) {
            break;
        }
    }
    Some(haveread)
}

/// `stream_supports_lock($stream): bool`,
/// `PHP_FUNCTION(stream_supports_lock)`
/// (`Z/ext/standard/streamsfuncs.c:1589`): whether the kind has
/// `flock(2)`, the plain files kind and the host's standard streams.
///
/// ```php
/// var_dump(stream_supports_lock(fopen(__FILE__, 'r')), stream_supports_lock(fopen('php://memory', 'r')), stream_supports_lock(fopen('php://stdout', 'w')));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn stream_supports_lock(req: &mut Request, stream: Stream) -> bool {
    stream.is_stdio(req)
}

/// `stream_set_write_buffer($stream, int $size): int` and its alias
/// `set_file_buffer`, `PHP_FUNCTION(stream_set_write_buffer)`
/// (`Z/ext/standard/streamsfuncs.c:1414`): `PHP_STREAM_OPTION_WRITE_BUFFER`,
/// `PHP_STREAM_BUFFER_NONE` for zero and `PHP_STREAM_BUFFER_FULL` with the
/// size otherwise, and `0` where the stream took it, `EOF` where not. A
/// plain file's takes it only over a `FILE*`, which none here is over,
/// memory has no such option, and a user-space stream is asked with
/// `stream_set_option`, `BUFSIZ` its size where none is given.
///
/// ```php
/// class W { public $context; function stream_open($p, $m, $o, &$op) { return true; }
///     function stream_set_option($o, $a, $b) { echo "option: $o, $a, $b\n"; return $o == 3 && $a == 2; } }
/// stream_wrapper_register('ww', 'W');
/// var_dump(stream_set_write_buffer(fopen(__FILE__, 'r'), 0), set_file_buffer(fopen('php://memory', 'r'), 100));
/// $w = fopen('ww://x', 'r');
/// var_dump(stream_set_write_buffer($w, 50), stream_set_read_buffer($w, 10));
/// ```
/// ```expect
/// int(-1)
/// int(-1)
/// option: 3, 2, 50
/// option: 2, 2, 10
/// int(0)
/// int(-1)
/// ```
#[php_function(aliases(set_file_buffer))]
fn stream_set_write_buffer(req: &mut Request, stream: Stream, size: i64) -> i64 {
    let option = StreamOption::WriteBuffer {
        buffered: size != 0,
        size: buffer_size(size),
    };
    option_answer(stream.set_option(req, option))
}

/// The size a buffer option hands on: the one given, or `BUFSIZ`, what
/// `user_stream_set_option` hands where `ptrparam` is `NULL`, for zero.
fn buffer_size(buff: i64) -> i64 {
    if buff == 0 {
        i64::from(libc::BUFSIZ)
    } else {
        buff
    }
}

/// `RETURN_LONG(ret == 0 ? 0 : EOF)`.
fn option_answer(answer: OptionAnswer) -> i64 {
    if answer == OptionAnswer::Ok { 0 } else { -1 }
}

/// `stream_set_read_buffer($stream, int $size): int`,
/// `PHP_FUNCTION(stream_set_read_buffer)`
/// (`Z/ext/standard/streamsfuncs.c:1471`): `PHP_STREAM_OPTION_READ_BUFFER`,
/// zero reading the kind directly and any other size buffering it again,
/// which php's generic layer takes for a kind that does not, so a kind
/// here answers `0`, and a user-space stream what its `stream_set_option`
/// says.
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "abc"); rewind($m);
/// var_dump(stream_set_read_buffer($m, 0), fread($m, 1), stream_set_read_buffer($m, 10), fread($m, 5));
/// ```
/// ```expect
/// int(0)
/// string(1) "a"
/// int(0)
/// string(2) "bc"
/// ```
#[php_function]
fn stream_set_read_buffer(req: &mut Request, stream: Stream, size: i64) -> i64 {
    let option = StreamOption::ReadBuffer {
        buffered: size != 0,
        size: buffer_size(size),
    };
    option_answer(stream.set_option(req, option))
}

/// `stream_set_blocking($stream, bool $enable): bool` and its alias
/// `socket_set_blocking`, `PHP_FUNCTION(stream_set_blocking)`
/// (`Z/ext/standard/streamsfuncs.c:1357`): `PHP_STREAM_OPTION_BLOCKING`,
/// which fails only as `-1`: `O_NONBLOCK` on a plain file, a pipe or a
/// socket, `stream_set_option` on a user-space stream, and `true` for
/// memory, which has no such option. A pipe set not to block answers a
/// read with nothing where it has nothing.
///
/// ```php
/// $p = popen('sleep 1', 'r');
/// var_dump(stream_set_blocking($p, false), fread($p, 10), stream_get_meta_data($p)['blocked']);
/// var_dump(stream_set_blocking(fopen('php://memory', 'r'), false), socket_set_blocking(fopen(__FILE__, 'r'), true));
/// pclose($p);
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function(aliases(socket_set_blocking))]
fn stream_set_blocking(req: &mut Request, stream: Stream, enable: bool) -> bool {
    let answer = stream.set_option(req, StreamOption::Blocking(enable));
    answer != OptionAnswer::Err
}

/// `stream_set_timeout($stream, int $seconds, int $microseconds = 0): bool`
/// and its alias `socket_set_timeout`, `PHP_FUNCTION(stream_set_timeout)`
/// (`Z/ext/standard/streamsfuncs.c:1374`): `PHP_STREAM_OPTION_READ_TIMEOUT`,
/// the microseconds carried into the seconds as php carries them, and
/// `true` only for `PHP_STREAM_OPTION_RETURN_OK`, which a socket and a
/// user-space stream whose `stream_set_option` says so answer.
///
/// ```php
/// var_dump(stream_set_timeout(fopen('php://memory', 'r'), 1), socket_set_timeout(fopen(__FILE__, 'r'), 1, 500));
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// var_dump(stream_set_timeout($s, 0, 1500000));
/// ```
/// ```expect
/// Deprecated: Function socket_set_timeout() is deprecated since 8.5, use stream_set_timeout() instead in %s on line 2
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn stream_set_timeout(
    req: &mut Request,
    stream: Stream,
    seconds: i64,
    #[php(default = 0)] microseconds: i64,
) -> bool {
    set_timeout(req, stream, seconds, microseconds)
}

/// The one body of `stream_set_timeout` and its deprecated alias.
fn set_timeout(req: &mut Request, stream: Stream, seconds: i64, microseconds: i64) -> bool {
    let seconds = seconds.saturating_add(microseconds / 1_000_000);
    let micros = microseconds % 1_000_000;
    let timeout = std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
        + std::time::Duration::from_micros(u64::try_from(micros).unwrap_or(0));
    let answer = stream.set_option(req, StreamOption::ReadTimeout(timeout));
    answer == OptionAnswer::Ok
}

/// `socket_set_timeout`, `ZEND_ACC_DEPRECATED`: the call raises
/// `zend_deprecated_function`'s sentence from the declaration before
/// the arguments are read, then this is `stream_set_timeout`.
///
/// ```php
/// var_dump(socket_set_timeout(fopen('php://memory', 'r'), 1));
/// ```
/// ```expect
/// Deprecated: Function socket_set_timeout() is deprecated since 8.5, use stream_set_timeout() instead in %s on line 2
/// bool(false)
/// ```
#[php_function(deprecated(since = "8.5", message = "use stream_set_timeout() instead"))]
fn socket_set_timeout(
    req: &mut Request,
    stream: Stream,
    seconds: i64,
    #[php(default = 0)] microseconds: i64,
) -> bool {
    set_timeout(req, stream, seconds, microseconds)
}

/// `stream_set_chunk_size($stream, int $size): int`,
/// `PHP_FUNCTION(stream_set_chunk_size)`
/// (`Z/ext/standard/streamsfuncs.c:1440`): a size at or below zero is
/// php's `ValueError`; else the chunk size the stream had, 8192 until
/// one is set, and the new one kept.
///
/// ```php
/// $m = fopen('php://memory', 'r');
/// var_dump(stream_set_chunk_size($m, 100), stream_set_chunk_size($m, 5), stream_set_chunk_size(fopen('php://memory', 'r'), 1));
/// try { stream_set_chunk_size($m, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(8192)
/// int(100)
/// int(8192)
/// stream_set_chunk_size(): Argument #2 ($size) must be greater than 0
/// ```
#[php_function]
fn stream_set_chunk_size(req: &mut Request, stream: Stream, size: i64) -> Result<i64, Thrown> {
    if size <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    // `RETURN_LONG(ret > 0 ? (zend_long)ret : (zend_long)EOF)`.
    let OptionAnswer::Previous(old) = stream.set_option(req, StreamOption::ChunkSize(size)) else {
        return Ok(-1);
    };
    // `stream->chunk_size > INT_MAX ? INT_MAX : (int)stream->chunk_size`.
    Ok(i64::try_from(old)
        .unwrap_or(i64::MAX)
        .min(i64::from(i32::MAX)))
}

/// `stream_get_line($stream, int $length, string $ending = ""): string|false`,
/// `PHP_FUNCTION(stream_get_line)` over `php_stream_get_record`
/// (`Z/main/streams/streams.c:1098`): a length below zero is php's
/// `ValueError` and zero is 8192; with no ending, up to that many bytes;
/// with one, the bytes before the first ending found within that many,
/// the ending consumed and not returned, or that many bytes where none
/// is; what is left at the end of the stream, and `false` where nothing
/// is, or where a stream that does not block has nothing more yet. The
/// stream's read buffer is filled a chunk at a time, for every kind, so
/// a filter appended after it sees the bytes it holds.
///
/// ```php
/// $m = fopen('php://memory', 'wb+'); fwrite($m, ".\r\n==="); rewind($m);
/// var_dump(stream_get_line($m, 8192, "\r\n"), stream_get_meta_data($m)['unread_bytes']);
/// var_dump(stream_filter_append($m, 'convert.base64-decode'));
/// ```
/// ```expect
/// string(1) "."
/// int(3)
///
/// Warning: stream_filter_append(): Stream filter (convert.base64-decode): invalid byte sequence in %s on line 4
///
/// Warning: stream_filter_append(): Filter failed to process pre-buffered data in %s on line 4
/// bool(false)
/// ```
///
/// ```php
/// $m = fopen('php://memory', 'r+'); fwrite($m, "a||b||c"); rewind($m);
/// var_dump(stream_get_line($m, 100, "||"), stream_get_line($m, 1, "||"), stream_get_line($m, 100, "||"), stream_get_line($m, 100, "||"), stream_get_line($m, 100, "||"));
/// rewind($m); var_dump(stream_get_line($m, 3), stream_get_line($m, 0));
/// try { stream_get_line($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "b"
/// string(0) ""
/// string(1) "c"
/// bool(false)
/// string(3) "a||"
/// string(4) "b||c"
/// stream_get_line(): Argument #2 ($length) must be greater than or equal to 0
/// ```
#[php_function]
fn stream_get_line(
    req: &mut Request,
    stream: Stream,
    length: i64,
    #[php(default = "")] ending: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let ending = ending.as_bytes().to_vec();
    if length < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let maxlen = if length == 0 {
        8192
    } else {
        usize::try_from(length).unwrap_or(usize::MAX)
    };
    Ok(stream
        .get_record(req, maxlen, &ending)?
        .map_or(OrFalse::False, OrFalse::Value))
}

/// `stream_resolve_include_path(string $filename): string|false`,
/// `PHP_FUNCTION(stream_resolve_include_path)` over `php_resolve_path`
/// (`Z/main/fopen_wrappers.c:500`): a path with a scheme other than
/// `file://` is not resolved; one that begins `./` or `../`, or is
/// absolute, is its realpath; otherwise each `include_path` entry in turn
/// with the name after it, then the directory of the file being served;
/// the first that resolves, or `false`.
///
/// ```php
/// $d = __DIR__ . '/inc-d'; @mkdir($d); touch("$d/in.php"); set_include_path("nope:$d");
/// var_dump(stream_resolve_include_path('in.php') === realpath("$d/in.php"), stream_resolve_include_path('./nope.php'), stream_resolve_include_path('nope.php'), stream_resolve_include_path('php://memory'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// An `include_path` entry of a wrapper is the wrapper's `url_stat`, the
/// path answered as it was joined; a throw of `url_stat` leaves the walk.
///
/// ```php
/// class W { public $context; function url_stat($path, $flags) { if (str_contains($path, 'boom')) throw new Exception("stat failed"); return str_contains($path, 'known') ? ['mode' => 0100644] : false; } }
/// stream_wrapper_register('vw', 'W');
/// set_include_path('vw://root/a' . PATH_SEPARATOR . '.');
/// var_dump(stream_resolve_include_path('path/known.php'), stream_resolve_include_path('path/other.php'));
/// try { stream_resolve_include_path('boom'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(26) "vw://root/a/path/known.php"
/// bool(false)
/// stat failed
/// ```
#[php_function]
fn stream_resolve_include_path(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let filename = filename.0.as_bytes().to_vec();
    // `zend_resolve_path`, which answers a regular file or a wrapper's
    // path; the walk below is what it leaves, a directory.
    match req.code_mut().resolve_path(&filename) {
        Ok(path) => return Ok(OrFalse::Value(PhpString::new(req, &path)?)),
        Err(Unresolved::Threw(thrown)) => return Err(thrown),
        Err(Unresolved::Missing(_) | Unresolved::Url(_)) => {}
    }
    let cwd = req.environment().cwd().as_os_str().as_bytes().to_vec();
    let realpath = |req: &mut Request, path: &[u8]| {
        let base: &[u8] = if path.first() == Some(&b'/') {
            b""
        } else {
            &cwd
        };
        req.environment_mut()
            .virtual_file_ex(base, path, Resolve::RealPath)
    };
    let scheme = filename
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')))
        .filter(|&at| at > 1 && filename.get(at..at + 3) == Some(b"://".as_slice()));
    let resolved = if let Some(at) = scheme {
        if filename[..at].eq_ignore_ascii_case(b"file") {
            filename.get(at + 3..).and_then(|path| realpath(req, path))
        } else {
            None
        }
    } else {
        let include_path = req.ini().get(b"include_path").map(<[u8]>::to_vec);
        let relative = filename.starts_with(b"./") || filename.starts_with(b"../");
        match include_path.filter(|p| !p.is_empty()) {
            Some(path) if !relative && filename.first() != Some(&b'/') => {
                let found = path
                    .split(|c| *c == b':')
                    .filter(|entry| entry.len() + 1 + filename.len() + 1 < MAXPATHLEN)
                    .find_map(|entry| {
                        let mut trypath = entry.to_vec();
                        trypath.push(b'/');
                        trypath.extend_from_slice(&filename);
                        realpath(req, &trypath)
                    });
                found.or_else(|| {
                    let executed = req.frames().current_file()?.to_vec();
                    let dir = executed.iter().rposition(|c| *c == b'/')?;
                    let mut trypath = executed.get(..=dir)?.to_vec();
                    trypath.extend_from_slice(&filename);
                    realpath(req, &trypath)
                })
            }
            _ => realpath(req, &filename),
        }
    };
    match resolved {
        Some(path) => Ok(OrFalse::Value(PhpString::new(req, &path)?)),
        None => Ok(OrFalse::False),
    }
}

/// `stream_get_wrappers(): array`, `PHP_FUNCTION(stream_get_wrappers)`
/// (`Z/ext/standard/streamsfuncs.c:583`): the schemes of the wrappers
/// the request has, in the order they were registered, with what a
/// program registered and without what it unregistered.
///
/// ```php
/// foreach (stream_get_wrappers() as $w) { if ($w === 'php' || $w === 'file') echo $w, "\n"; }
/// class Foo {}
/// stream_wrapper_register('foo', 'Foo');
/// var_dump(in_array('foo', stream_get_wrappers()));
/// stream_wrapper_unregister('file');
/// var_dump(in_array('file', stream_get_wrappers()));
/// stream_wrapper_restore('file');
/// var_dump(in_array('file', stream_get_wrappers()));
/// ```
/// ```expect
/// php
/// file
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn stream_get_wrappers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for scheme in req.stream_wrappers() {
        let scheme = PhpString::new(req, scheme.as_bytes())?.into_value();
        array.append(req, scheme)?;
    }
    Ok(array)
}

/// `stream_is_local($stream): bool`, `PHP_FUNCTION(stream_is_local)`
/// (`Z/ext/standard/streamsfuncs.c:1563`): `wrapper && wrapper->is_url ==
/// 0`, the stream's wrapper or the one `php_stream_locate_url_wrapper`
/// finds for a path. A stream no wrapper opened, a pipe or a socket, and a
/// path no wrapper serves are not local, and a scheme nobody registered is
/// warned of and read as a plain file.
///
/// ```php
/// var_dump(stream_is_local(fopen('php://memory', 'r')), stream_is_local('/tmp'), stream_is_local('php://memory'), stream_is_local('file:///tmp'), stream_is_local('file://host/x'), stream_is_local('foo://x'), stream_is_local(1));
/// ```
/// ```expect
/// Warning: stream_is_local(): Unable to find the wrapper "foo" - did you forget to enable it when you configured PHP? in %s on line 2
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
///
/// A URL wrapper's path is not local, whatever registered it; symfony's
/// `FileLoader` refuses a resource that is, on this answer.
///
/// ```php
/// class W { public $context; function stream_open($p, $m, $o, &$op) { return true; } function stream_eof() { return true; } }
/// stream_wrapper_register('here', 'W'); stream_wrapper_register('there', 'W', STREAM_IS_URL);
/// var_dump(stream_is_local('http://example.com/a'), stream_is_local('HTTPS://a'), stream_is_local('ftp://a'), stream_is_local('data:text/plain,a'), stream_is_local('glob://a'), stream_is_local('compress.zlib://a'), stream_is_local('phar://a'), stream_is_local('here://a'), stream_is_local('there://a'));
/// var_dump(stream_is_local(fopen('here://a', 'r')), stream_is_local(fopen('there://a', 'r')));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
///
/// The stream's own wrapper is what a resource is read by: `tmpfile()` and
/// a directory are the plain files wrapper's, `data:` a URL wrapper's, and
/// a pipe and a socket were opened by none.
///
/// ```php
/// var_dump(stream_is_local(tmpfile()), stream_is_local(opendir('.')), stream_is_local(fopen('php://temp', 'r')), stream_is_local(STDIN));
/// var_dump(stream_is_local(fopen('data:text/plain,a', 'r')), stream_is_local(popen('true', 'r')), stream_is_local(stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0)[0]));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// A closed stream and another resource are refused as
/// `php_stream_from_zval` refuses them, and a path that names a wrapper the
/// request took out is not local.
///
/// ```php
/// $f = fopen('php://memory', 'r'); fclose($f);
/// foreach ([$f, stream_context_create()] as $r) {
///     try { stream_is_local($r); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// stream_wrapper_unregister('file');
/// var_dump(stream_is_local('/tmp'));
/// ```
/// ```expect
/// stream_is_local(): supplied resource is not a valid stream resource
/// stream_is_local(): supplied resource is not a valid stream resource
/// bool(false)
/// ```
#[php_function]
fn stream_is_local(req: &mut Request, stream: Untyped<Value>) -> Result<bool, Thrown> {
    let given = stream.0;
    let is_url = if let ValueRef::Resource(_) = given.as_ref() {
        // `php_stream_from_zval`.
        let Some(stream) = Stream::from_value(req, &given) else {
            let message = "stream_is_local(): supplied resource is not a valid stream resource";
            return Err(type_error(req, message));
        };
        stream.is_url(req)
    } else {
        let path = with_string(req, &given, <[u8]>::to_vec)?;
        req.path_is_url(&path)
    };
    Ok(is_url == Some(false))
}

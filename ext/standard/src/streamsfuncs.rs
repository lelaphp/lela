//! `Z/ext/standard/streamsfuncs.c`, the natives over the streams facet
//! that need no socket and no filter chain: the stream `contexts`, as
//! resources of php's `stream-context` type this crate registers per
//! request (`php_stream_context_alloc`), with their options and their
//! notification callback; `stream_get_contents`, `stream_copy_to_stream`
//! and `stream_get_line` over the stream's reads; the options
//! `stream_set_*` asks of a stream, answered as the kinds this engine has
//! answer them; and `stream_resolve_include_path`, `stream_is_local`,
//! `stream_supports_lock` and `stream_get_wrappers`. `stream_get_meta_data`
//! is `file`'s; `socket_get_status` is its alias here. The socket
//! natives, `stream_select`, the filter natives, `stream_isatty` and
//! `stream_get_transports` wait on faces the api does not have (#684).
//!
//! A stream opened without a `context` of its own uses php's default
//! one, `FG(default_context)`, which is what `stream_context_get_options`
//! reads for a stream here: the facet keeps no `context` on a stream.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::ffi::OsStrExt;

use php_ext::coerce::{callable_checked, value_name};
use php_ext::decl::{Attribute, AttributeArgument, ConstDecl, ConstExpr, FunctionDecl, Name};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, deprecated, docref, throw};
use php_ext::request::{OptionAnswer, Request, Resolve, Stream, StreamOption, Whence};
use php_ext::values::{Key, PhpArray, PhpString, Resource, ResourceType, Value, ValueRef};

use crate::file::stream_argument;
use crate::md5::path_argument;
use crate::output::Entry;
use crate::stub::stub;
use crate::virtual_cwd::MAXPATHLEN;
use crate::zpp::{
    ArrayOr, array_argument, array_or_string, given, nullable_array, nullable_long,
    nullable_string, optional_long,
};
use crate::{
    State, argument_count_between, bool_argument, constant, long_argument, string_argument,
};

/// What the request keeps for the `contexts`: the resource type, once
/// registered, and `FG(default_context)`.
#[derive(Default)]
pub(crate) struct Contexts {
    kind: Option<ResourceType>,
    default: Option<Resource>,
}

/// `php_stream_context`: its options array and its notifier's callback.
struct Context {
    options: Option<PhpArray>,
    notification: Option<Value>,
}

/// `PHP_STREAM_COPY_ALL` as `stream_get_contents` spells it, `-1`.
const COPY_ALL: i64 = -1;

/// The `STREAM_*` constants of `file.stub.php` the streams natives read
/// or hand back, in its order, with this host's values where the stub
/// takes a header's.
///
/// ```php
/// var_dump(STREAM_NOTIFY_CONNECT, STREAM_NOTIFY_RESOLVE, STREAM_NOTIFY_SEVERITY_ERR, STREAM_FILTER_ALL, STREAM_CLIENT_CONNECT, STREAM_CRYPTO_METHOD_ANY_CLIENT, STREAM_CRYPTO_METHOD_TLSv1_3_SERVER, STREAM_CRYPTO_PROTO_TLSv1_2, STREAM_SHUT_RDWR, STREAM_PF_INET6, STREAM_PF_UNIX, STREAM_IPPROTO_RAW, STREAM_SOCK_SEQPACKET, STREAM_SOCK_RDM, STREAM_PEEK, STREAM_OOB, STREAM_SERVER_BIND, STREAM_SERVER_LISTEN);
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(127)
/// int(64)
/// int(32)
/// int(2)
/// int(30)
/// int(1)
/// int(255)
/// int(5)
/// int(4)
/// int(2)
/// int(1)
/// int(4)
/// int(8)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    [
        ("STREAM_NOTIFY_CONNECT", 2),
        ("STREAM_NOTIFY_AUTH_REQUIRED", 3),
        ("STREAM_NOTIFY_AUTH_RESULT", 10),
        ("STREAM_NOTIFY_MIME_TYPE_IS", 4),
        ("STREAM_NOTIFY_FILE_SIZE_IS", 5),
        ("STREAM_NOTIFY_REDIRECTED", 6),
        ("STREAM_NOTIFY_PROGRESS", 7),
        ("STREAM_NOTIFY_FAILURE", 9),
        ("STREAM_NOTIFY_COMPLETED", 8),
        ("STREAM_NOTIFY_RESOLVE", 1),
        ("STREAM_NOTIFY_SEVERITY_INFO", 0),
        ("STREAM_NOTIFY_SEVERITY_WARN", 1),
        ("STREAM_NOTIFY_SEVERITY_ERR", 2),
        ("STREAM_FILTER_READ", 1),
        ("STREAM_FILTER_WRITE", 2),
        ("STREAM_FILTER_ALL", 3),
        ("STREAM_CLIENT_PERSISTENT", 1),
        ("STREAM_CLIENT_ASYNC_CONNECT", 2),
        ("STREAM_CLIENT_CONNECT", 4),
        ("STREAM_CRYPTO_METHOD_ANY_CLIENT", 127),
        ("STREAM_CRYPTO_METHOD_SSLv2_CLIENT", 3),
        ("STREAM_CRYPTO_METHOD_SSLv3_CLIENT", 5),
        ("STREAM_CRYPTO_METHOD_SSLv23_CLIENT", 57),
        ("STREAM_CRYPTO_METHOD_TLS_CLIENT", 121),
        ("STREAM_CRYPTO_METHOD_TLSv1_0_CLIENT", 9),
        ("STREAM_CRYPTO_METHOD_TLSv1_1_CLIENT", 17),
        ("STREAM_CRYPTO_METHOD_TLSv1_2_CLIENT", 33),
        ("STREAM_CRYPTO_METHOD_TLSv1_3_CLIENT", 65),
        ("STREAM_CRYPTO_METHOD_ANY_SERVER", 126),
        ("STREAM_CRYPTO_METHOD_SSLv2_SERVER", 2),
        ("STREAM_CRYPTO_METHOD_SSLv3_SERVER", 4),
        ("STREAM_CRYPTO_METHOD_SSLv23_SERVER", 120),
        ("STREAM_CRYPTO_METHOD_TLS_SERVER", 120),
        ("STREAM_CRYPTO_METHOD_TLSv1_0_SERVER", 8),
        ("STREAM_CRYPTO_METHOD_TLSv1_1_SERVER", 16),
        ("STREAM_CRYPTO_METHOD_TLSv1_2_SERVER", 32),
        ("STREAM_CRYPTO_METHOD_TLSv1_3_SERVER", 64),
        ("STREAM_CRYPTO_PROTO_SSLv3", 4),
        ("STREAM_CRYPTO_PROTO_TLSv1_0", 8),
        ("STREAM_CRYPTO_PROTO_TLSv1_1", 16),
        ("STREAM_CRYPTO_PROTO_TLSv1_2", 32),
        ("STREAM_CRYPTO_PROTO_TLSv1_3", 64),
        ("STREAM_SHUT_RD", 0),
        ("STREAM_SHUT_WR", 1),
        ("STREAM_SHUT_RDWR", 2),
        ("STREAM_PF_INET", 2),
        ("STREAM_PF_INET6", 30),
        ("STREAM_PF_UNIX", 1),
        ("STREAM_IPPROTO_IP", 0),
        ("STREAM_IPPROTO_TCP", 6),
        ("STREAM_IPPROTO_UDP", 17),
        ("STREAM_IPPROTO_ICMP", 1),
        ("STREAM_IPPROTO_RAW", 255),
        ("STREAM_SOCK_STREAM", 1),
        ("STREAM_SOCK_DGRAM", 2),
        ("STREAM_SOCK_RAW", 3),
        ("STREAM_SOCK_SEQPACKET", 5),
        ("STREAM_SOCK_RDM", 4),
        ("STREAM_PEEK", 2),
        ("STREAM_OOB", 1),
        ("STREAM_SERVER_BIND", 4),
        ("STREAM_SERVER_LISTEN", 8),
    ]
    .into_iter()
    .map(|(name, value)| constant(name.as_bytes(), value))
    .collect()
}

/// The natives, as the stub declares them.
///
/// ```php
/// foreach (['stream_context_create', 'stream_context_set_option', 'stream_get_contents', 'stream_copy_to_stream', 'stream_get_line', 'stream_set_blocking', 'socket_get_status'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function stream_context_create ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> ?array $options = null ]
///     Parameter #1 [ <optional> ?array $params = null ]
///   }
/// }
/// Function [ <internal:standard> function stream_context_set_option ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $context ]
///     Parameter #1 [ <required> array|string $wrapper_or_options ]
///     Parameter #2 [ <optional> ?string $option_name = null ]
///     Parameter #3 [ <optional> mixed $value = <default> ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function stream_get_contents ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <optional> ?int $length = null ]
///     Parameter #2 [ <optional> int $offset = -1 ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function stream_copy_to_stream ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $from ]
///     Parameter #1 [ <required> $to ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> int $offset = 0 ]
///   }
///   - Return [ int|false ]
/// }
/// Function [ <internal:standard> function stream_get_line ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> int $length ]
///     Parameter #2 [ <optional> string $ending = "" ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function stream_set_blocking ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> bool $enable ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function socket_get_status ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $stream ]
///   }
///   - Return [ array ]
/// }
/// ```
#[allow(clippy::too_many_lines)] // one row per native of the stub
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub("stream_context_create(?array $options = null, ?array $params = null)"),
            stream_context_create as Entry,
        ),
        (
            stub("stream_context_set_params($context, array $params): true"),
            stream_context_set_params,
        ),
        (
            stub("stream_context_get_params($context): array"),
            stream_context_get_params,
        ),
        (
            stub(
                "stream_context_set_option($context, array|string $wrapper_or_options, ?string $option_name = null, mixed $value = UNKNOWN): true",
            ),
            stream_context_set_option,
        ),
        (
            stub("stream_context_set_options($context, array $options): true"),
            stream_context_set_options,
        ),
        (
            stub("stream_context_get_options($stream_or_context): array"),
            stream_context_get_options,
        ),
        (
            stub("stream_context_get_default(?array $options = null)"),
            stream_context_get_default,
        ),
        (
            stub("stream_context_set_default(array $options)"),
            stream_context_set_default,
        ),
        (
            stub(
                "stream_copy_to_stream($from, $to, ?int $length = null, int $offset = 0): int|false",
            ),
            stream_copy_to_stream,
        ),
        (
            stub(
                "stream_get_contents($stream, ?int $length = null, int $offset = -1): string|false",
            ),
            stream_get_contents,
        ),
        (
            stub("stream_supports_lock($stream): bool"),
            stream_supports_lock,
        ),
        (
            stub("stream_set_write_buffer($stream, int $size): int"),
            stream_set_write_buffer,
        ),
        (
            stub("set_file_buffer($stream, int $size): int"),
            stream_set_write_buffer,
        ),
        (
            stub("stream_set_read_buffer($stream, int $size): int"),
            stream_set_read_buffer,
        ),
        (
            stub("stream_set_blocking($stream, bool $enable): bool"),
            stream_set_blocking,
        ),
        (
            stub("socket_set_blocking($stream, bool $enable): bool"),
            stream_set_blocking,
        ),
        (
            stub("socket_get_status($stream): array"),
            crate::file::stream_get_meta_data,
        ),
        (
            stub("stream_get_line($stream, int $length, string $ending = \"\"): string|false"),
            stream_get_line,
        ),
        (
            stub("stream_resolve_include_path(string $filename): string|false"),
            stream_resolve_include_path,
        ),
        (stub("stream_get_wrappers(): array"), stream_get_wrappers),
        (stub("stream_is_local($stream): bool"), stream_is_local),
        (
            stub("stream_set_chunk_size($stream, int $size): int"),
            stream_set_chunk_size,
        ),
        (
            stub("stream_set_timeout($stream, int $seconds, int $microseconds = 0): bool"),
            stream_set_timeout,
        ),
        (
            deprecated_alias(
                stub("socket_set_timeout($stream, int $seconds, int $microseconds = 0): bool"),
                "use stream_set_timeout() instead",
            ),
            socket_set_timeout,
        ),
    ]
}

/// The declaration under `#[\Deprecated(since: '8.5', message: ...)]`,
/// as the stub marks `socket_set_timeout`.
fn deprecated_alias(decl: FunctionDecl, message: &str) -> FunctionDecl {
    let argument = |name: &[u8], value: &str| AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    FunctionDecl {
        attributes: vec![Attribute {
            name: Name(b"Deprecated".as_slice().into()),
            arguments: vec![argument(b"since", "8.5"), argument(b"message", message)],
        }],
        ..decl
    }
}

/// `le_stream_context`'s destructor, `php_stream_context_free`: the
/// options and the callback given back.
fn context_dtor(req: &mut Request, res: Resource) {
    let taken = res
        .native_mut::<Context>(req)
        .map(|context| (context.options.take(), context.notification.take()));
    if let Some((options, notification)) = taken {
        if let Some(options) = options {
            req.release(Value::array(options));
        }
        if let Some(notification) = notification {
            req.release(notification);
        }
    }
}

/// `php_le_stream_context()`: the type, registered on first use.
fn context_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().contexts.kind {
        return kind;
    }
    let kind = req
        .resources()
        .register_type(b"stream-context", Some(context_dtor));
    req.state::<State>().contexts.kind = Some(kind);
    kind
}

/// `php_stream_context_alloc`: a `context` with an empty options array,
/// a resource of its own with one owner, the caller's.
fn context_alloc(req: &mut Request) -> Result<Resource, Thrown> {
    let kind = context_type(req);
    let options = PhpArray::new(req)?;
    Resource::new(
        req,
        kind,
        Context {
            options: Some(options),
            notification: None,
        },
    )
}

/// `FG(default_context)`, made on first use and kept for the request.
fn default_context(req: &mut Request) -> Result<Resource, Thrown> {
    if let Some(res) = req.state::<State>().contexts.default {
        return Ok(res);
    }
    let res = context_alloc(req)?;
    req.state::<State>().contexts.default = Some(res);
    Ok(res)
}

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
/// (`Z/main/streams/php_stream_context.h:36`) over the `$context` at
/// `position`, which `Z_PARAM_RESOURCE_OR_NULL` has taken: the one given,
/// which must be a `stream-context` (`zend_fetch_resource_ex`'s
/// `supplied resource is not a valid Stream-Context resource`), or for
/// `null` `FG(default_context)`, made on the first use and a resource of
/// the request's from then, unless `nocontext` says none. The first open
/// of a request that is handed no `$context` makes it, so under the CLI
/// it is resource 4, after the three standard streams, and the stream is
/// 5. What it answers is the `php_stream_context` the native's stream
/// operations are handed ([`Request::with_stream_ctx`]).
///
/// ```php
/// var_dump(fopen('php://memory', 'r'), stream_context_get_default(), fopen('php://memory', 'r'));
/// try { file_get_contents(__FILE__, false, STDIN); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// resource(5) of type (stream)
/// resource(4) of type (stream-context)
/// resource(6) of type (stream)
/// file_get_contents(): supplied resource is not a valid Stream-Context resource
/// ```
pub(crate) fn stream_context(
    req: &mut Request,
    position: u32,
    nocontext: NullContext,
) -> Result<Option<Resource>, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    if let Some(res) = given.as_resource().copied() {
        let kind = context_type(req);
        if res.is_closed() || res.kind() != Some(kind) {
            let text = format!(
                "{}(): supplied resource is not a valid Stream-Context resource",
                req.active_function_name()
            );
            return Err(throw(req, "TypeError", &text));
        }
        return Ok(Some(res));
    }
    if nocontext == NullContext::Default {
        return Ok(Some(default_context(req)?));
    }
    Ok(None)
}

/// `Z_PARAM_RESOURCE` then `decode_context_param`
/// (`Z/ext/standard/streamsfuncs.c:937`): a `context`, or a stream's,
/// which here is the default one; anything else is php's
/// `must be a valid stream/context`.
fn context_argument(req: &mut Request, position: u32) -> Result<Resource, Thrown> {
    let given = req.argument(position).unwrap_or_default().deref();
    let Some(res) = given.as_resource().copied() else {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            position,
            &format!("be of type resource, {name} given"),
        ));
    };
    let kind = context_type(req);
    if !res.is_closed() && res.kind() == Some(kind) {
        return Ok(res);
    }
    if Stream::from_value(req, &given).is_some() {
        return default_context(req);
    }
    Err(arg_type_error(req, position, "be a valid stream/context"))
}

/// `php_stream_context_set_option` (`Z/main/streams/streams.c:2457`):
/// both levels separated, the wrapper's array made where it is missing,
/// and the value, dereferenced, written under the option's name; each
/// name as the C string it is, up to its first NUL.
fn set_option(
    req: &mut Request,
    res: Resource,
    wrapper: &[u8],
    option: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    let Some(mut options) = res
        .native_mut::<Context>(req)
        .and_then(|context| context.options.take())
    else {
        return Ok(());
    };
    let wrote = put_option(req, &mut options, wrapper, option, value);
    if let Some(context) = res.native_mut::<Context>(req) {
        context.options = Some(options);
    }
    wrote
}

/// The body of [`set_option`] over the options array taken out of the
/// `context` for the call.
fn put_option(
    req: &mut Request,
    options: &mut PhpArray,
    wrapper: &[u8],
    option: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    let c_string = |bytes: &[u8]| bytes.split(|c| *c == 0).next().unwrap_or(&[]).to_vec();
    options.separate(req)?;
    let wrapper_key = Key::Str(PhpString::new(req, &c_string(wrapper))?);
    let existing = options.get(&wrapper_key).map(|v| v.deref());
    let mut category = match existing.as_ref().and_then(Value::as_array) {
        Some(array) => {
            let array = *array;
            req.addref(&Value::array(array));
            array
        }
        None => PhpArray::new(req)?,
    };
    category.separate(req)?;
    let value = value.deref();
    req.addref(&value);
    let option_key = Key::Str(PhpString::new(req, &c_string(option))?);
    category.set(req, option_key, value)?;
    options.set(req, wrapper_key, Value::array(category))
}

/// `parse_context_options` (`Z/ext/standard/streamsfuncs.c:872`): each
/// string key holding an array sets its string-keyed options, in order;
/// the first entry that is neither is php's `ValueError`, what came
/// before it already set.
fn parse_options(req: &mut Request, res: Resource, options: PhpArray) -> Result<(), Thrown> {
    for (wkey, wval) in options.snapshot() {
        let wval = wval.deref();
        match (&wkey, wval.as_array()) {
            (Key::Str(wrapper), Some(inner)) => {
                for (okey, oval) in inner.snapshot() {
                    if let Key::Str(option) = okey {
                        set_option(req, res, wrapper.as_bytes(), option.as_bytes(), &oval)?;
                    }
                }
            }
            _ => {
                return Err(throw(
                    req,
                    "ValueError",
                    "Options should have the form [\"wrappername\"][\"optionname\"] = $value",
                ));
            }
        }
    }
    Ok(())
}

/// `parse_context_params` (`Z/ext/standard/streamsfuncs.c:896`):
/// `notification` a callable, php's `TypeError` otherwise, which
/// replaces the one there; `options` an array of options, php's
/// `Invalid stream/context parameter` otherwise.
fn parse_params(req: &mut Request, res: Resource, params: PhpArray) -> Result<(), Thrown> {
    let notification = Key::Str(PhpString::new(req, b"notification")?);
    if let Some(callback) = params.get(&notification) {
        if let Err(why) = callable_checked(req, &callback) {
            return Err(arg_type_error(
                req,
                1,
                &format!("be an array with valid callbacks as values, {why}"),
            ));
        }
        req.addref(&callback);
        let old = res
            .native_mut::<Context>(req)
            .and_then(|context| context.notification.replace(callback));
        if let Some(old) = old {
            req.release(old);
        }
    }
    let options = Key::Str(PhpString::new(req, b"options")?);
    if let Some(options) = params.get(&options) {
        let options = options.deref();
        let Some(options) = options.as_array() else {
            return Err(throw(req, "TypeError", "Invalid stream/context parameter"));
        };
        return parse_options(req, res, *options);
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
fn stream_context_create(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 2)?;
    let options = nullable_array(req, 1)?;
    let params = nullable_array(req, 2)?;
    let res = context_alloc(req)?;
    let mut applied = Ok(());
    if let Some(options) = options {
        applied = parse_options(req, res, options);
    }
    if let (Ok(()), Some(params)) = (&applied, params) {
        applied = parse_params(req, res, params);
    }
    match applied {
        Ok(()) => Ok(res.as_value()),
        Err(thrown) => {
            req.release(res.as_value());
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
fn stream_context_set_params(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let res = context_argument(req, 1)?;
    let params = array_argument(req, 2)?;
    parse_params(req, res, params)?;
    Ok(Value::bool(true))
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
fn stream_context_get_params(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let res = context_argument(req, 1)?;
    let (options, notification) = res.native::<Context>(req).map_or((None, None), |context| {
        (context.options, context.notification)
    });
    let mut array = PhpArray::new(req)?;
    if let Some(notification) = notification {
        req.addref(&notification);
        let key = Key::Str(PhpString::new(req, b"notification")?);
        array.set(req, key, notification)?;
    }
    let options = match options {
        Some(options) => {
            let value = Value::array(options);
            req.addref(&value);
            value
        }
        None => Value::array(PhpArray::new(req)?),
    };
    let key = Key::Str(PhpString::new(req, b"options")?);
    array.set(req, key, options)?;
    Ok(Value::array(array))
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
fn stream_context_set_option(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() == 2 {
        deprecated(
            req,
            "Calling stream_context_set_option() with 2 arguments is deprecated, use stream_context_set_options() instead",
        );
    }
    argument_count_between(req, 2, 4)?;
    let res = context_argument(req, 1)?;
    let wrapper_or_options = array_or_string(req, 2, false)?;
    let option = nullable_string(req, 3)?;
    let value = given(req, 4);
    let refuse = |req: &mut Request, text: &str| {
        let message = format!("stream_context_set_option(): {text}");
        Err(throw(req, "ValueError", &message))
    };
    match wrapper_or_options {
        Some(ArrayOr::Array(options)) => {
            if option.is_some() {
                return refuse(
                    req,
                    "Argument #3 ($option_name) must be null when argument #2 ($wrapper_or_options) is an array",
                );
            }
            if value.is_some() {
                return refuse(
                    req,
                    "Argument #4 ($value) cannot be provided when argument #2 ($wrapper_or_options) is an array",
                );
            }
            parse_options(req, res, options)?;
        }
        Some(ArrayOr::Other(wrapper)) => {
            let Some(option) = option else {
                return refuse(
                    req,
                    "Argument #3 ($option_name) cannot be null when argument #2 ($wrapper_or_options) is a string",
                );
            };
            let Some(value) = value else {
                return refuse(
                    req,
                    "Argument #4 ($value) must be provided when argument #2 ($wrapper_or_options) is a string",
                );
            };
            set_option(req, res, wrapper.as_bytes(), option.as_bytes(), &value)?;
        }
        None => {}
    }
    Ok(Value::bool(true))
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
fn stream_context_set_options(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let res = context_argument(req, 1)?;
    let options = array_argument(req, 2)?;
    parse_options(req, res, options)?;
    Ok(Value::bool(true))
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
fn stream_context_get_options(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let res = context_argument(req, 1)?;
    let options = res
        .native::<Context>(req)
        .and_then(|context| context.options);
    match options {
        Some(options) => {
            let value = Value::array(options);
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::array(PhpArray::new(req)?)),
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
fn stream_context_get_default(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 1)?;
    let options = nullable_array(req, 1)?;
    let res = default_context(req)?;
    if let Some(options) = options {
        parse_options(req, res, options)?;
    }
    let value = res.as_value();
    req.addref(&value);
    Ok(value)
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
fn stream_context_set_default(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let options = array_argument(req, 1)?;
    let res = default_context(req)?;
    parse_options(req, res, options)?;
    let value = res.as_value();
    req.addref(&value);
    Ok(value)
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
fn stream_get_contents(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let stream = stream_argument(req, 1)?;
    let maxlen = nullable_long(req, 2)?.unwrap_or(COPY_ALL);
    let desiredpos = optional_long(req, 3, -1)?;
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
            return Ok(Value::bool(false));
        }
    }
    let maxlen = (maxlen != COPY_ALL).then(|| usize::try_from(maxlen).unwrap_or(0));
    if maxlen == Some(0) {
        return Ok(Value::string(PhpString::new(req, b"")?));
    }
    Ok(Value::string(stream.copy_to_mem(req, maxlen)?))
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
fn stream_copy_to_stream(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let src = stream_argument(req, 1)?;
    let dest = stream_argument(req, 2)?;
    let maxlen = nullable_long(req, 3)?;
    let pos = optional_long(req, 4, 0)?;
    if pos > 0 && !src.seek(req, pos, Whence::Set) {
        let text = format!("Failed to seek to position {pos} in the stream");
        docref(req, Level::Warning, &text);
        return Ok(Value::bool(false));
    }
    // `PHP_STREAM_COPY_ALL`, and any other length the `size_t` makes
    // larger than a stream holds, is no limit.
    let maxlen = match maxlen {
        None => None,
        Some(0) => return Ok(Value::int(0)),
        Some(n) => usize::try_from(n).ok(),
    };
    let mut haveread: usize = 0;
    loop {
        let readchunk = maxlen.map_or(8192, |max| (max - haveread).min(8192));
        let Some(chunk) = src.read(req, readchunk)? else {
            return Ok(Value::bool(false));
        };
        if chunk.is_empty() {
            break;
        }
        let bytes = chunk.as_bytes().to_vec();
        req.release(Value::string(chunk));
        let mut written = 0;
        while written < bytes.len() {
            match dest.write(req, &bytes[written..]) {
                Some(n) if n > 0 => written += n,
                _ => return Ok(Value::bool(false)),
            }
        }
        haveread += bytes.len();
        if maxlen.is_some_and(|max| haveread >= max) {
            break;
        }
    }
    Ok(Value::int(i64::try_from(haveread).unwrap_or(i64::MAX)))
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
fn stream_supports_lock(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let stream = stream_argument(req, 1)?;
    Ok(Value::bool(stream.is_stdio(req)))
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
fn stream_set_write_buffer(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let buff = long_argument(req, 2)?;
    let option = StreamOption::WriteBuffer {
        buffered: buff != 0,
        size: buffer_size(buff),
    };
    Ok(Value::int(option_answer(stream.set_option(req, option))))
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
fn stream_set_read_buffer(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let buff = long_argument(req, 2)?;
    let option = StreamOption::ReadBuffer {
        buffered: buff != 0,
        size: buffer_size(buff),
    };
    Ok(Value::int(option_answer(stream.set_option(req, option))))
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
fn stream_set_blocking(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let block = bool_argument(req, 2)?;
    let answer = stream.set_option(req, StreamOption::Blocking(block));
    Ok(Value::bool(answer != OptionAnswer::Err))
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
fn stream_set_timeout(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let stream = stream_argument(req, 1)?;
    let seconds = long_argument(req, 2)?;
    let microseconds = optional_long(req, 3, 0)?;
    let seconds = seconds.saturating_add(microseconds / 1_000_000);
    let micros = microseconds % 1_000_000;
    let timeout = std::time::Duration::from_secs(u64::try_from(seconds).unwrap_or(0))
        + std::time::Duration::from_micros(u64::try_from(micros).unwrap_or(0));
    let answer = stream.set_option(req, StreamOption::ReadTimeout(timeout));
    Ok(Value::bool(answer == OptionAnswer::Ok))
}

/// `socket_set_timeout`, `ZEND_ACC_DEPRECATED`: `zend_deprecated_function`'s
/// sentence before the arguments are read, then `stream_set_timeout`.
fn socket_set_timeout(req: &mut Request) -> Result<Value, Thrown> {
    deprecated(
        req,
        "Function socket_set_timeout() is deprecated since 8.5, use stream_set_timeout() instead",
    );
    stream_set_timeout(req)
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
fn stream_set_chunk_size(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let stream = stream_argument(req, 1)?;
    let size = long_argument(req, 2)?;
    if size <= 0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    let OptionAnswer::Previous(old) = stream.set_option(req, StreamOption::ChunkSize(size)) else {
        return Ok(Value::bool(false));
    };
    // `stream->chunk_size > INT_MAX ? INT_MAX : (int)stream->chunk_size`.
    Ok(Value::int(
        i64::try_from(old)
            .unwrap_or(i64::MAX)
            .min(i64::from(i32::MAX)),
    ))
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
fn stream_get_line(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 3)?;
    let stream = stream_argument(req, 1)?;
    let length = long_argument(req, 2)?;
    let ending = match given(req, 3) {
        Some(_) => string_argument(req, 3)?.as_bytes().to_vec(),
        None => Vec::new(),
    };
    if length < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    let maxlen = if length == 0 {
        8192
    } else {
        usize::try_from(length).unwrap_or(usize::MAX)
    };
    match stream.get_record(req, maxlen, &ending)? {
        Some(record) => Ok(Value::string(record)),
        None => Ok(Value::bool(false)),
    }
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
fn stream_resolve_include_path(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let filename = path_argument(req, 1)?;
    let filename = filename.as_bytes().to_vec();
    let cwd = req.cwd().0.as_os_str().as_bytes().to_vec();
    let realpath = |req: &mut Request, path: &[u8]| {
        let base: &[u8] = if path.first() == Some(&b'/') {
            b""
        } else {
            &cwd
        };
        req.virtual_file_ex(base, path, Resolve::RealPath)
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
        let include_path = req.ini_get(b"include_path").map(<[u8]>::to_vec);
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
                    let executed = req.current_file()?.to_vec();
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
        Some(path) => Ok(Value::string(PhpString::new(req, &path)?)),
        None => Ok(Value::bool(false)),
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
fn stream_get_wrappers(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let mut array = PhpArray::new(req)?;
    for scheme in req.stream_wrappers() {
        let scheme = Value::string(PhpString::new(req, scheme.as_bytes())?);
        array.append(req, scheme)?;
    }
    Ok(Value::array(array))
}

/// `stream_is_local($stream): bool`, `PHP_FUNCTION(stream_is_local)`
/// (`Z/ext/standard/streamsfuncs.c:1563`): whether the wrapper is not a
/// URL wrapper. Every stream here is a local wrapper's; a path's wrapper
/// is found as `php_stream_locate_url_wrapper` finds it, a scheme no
/// wrapper serves warned of and taken as a plain file, and `file://`
/// naming a host no wrapper at all.
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
fn stream_is_local(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    if let ValueRef::Resource(_) = given.as_ref() {
        stream_argument(req, 1)?;
        return Ok(Value::bool(true));
    }
    let path = php_ext::coerce::to_string(req, &given)?;
    let path = path.as_bytes();
    let n = path
        .iter()
        .position(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')))
        .unwrap_or(path.len());
    let rest = path.get(n..).unwrap_or_default();
    let protocol = n > 1
        && rest.first() == Some(&b':')
        && (rest.get(1..3) == Some(b"//".as_slice()) || (n == 4 && path.starts_with(b"data:")));
    if !protocol {
        return Ok(Value::bool(true));
    }
    let scheme = &path[..n];
    let known = req
        .stream_wrappers()
        .iter()
        .any(|w| w.as_bytes().eq_ignore_ascii_case(scheme));
    if !known {
        let name = String::from_utf8_lossy(scheme.get(..31).unwrap_or(scheme)).into_owned();
        docref(
            req,
            Level::Warning,
            &format!(
                "Unable to find the wrapper \"{name}\" - did you forget to enable it when you configured PHP?"
            ),
        );
        return Ok(Value::bool(true));
    }
    if scheme.eq_ignore_ascii_case(b"file") {
        let localhost = path.len() >= 17 && path[..17].eq_ignore_ascii_case(b"file://localhost/");
        let after = path.get(n + 3).copied();
        if !localhost && after.is_some_and(|c| c != b'/') {
            return Ok(Value::bool(false));
        }
    }
    Ok(Value::bool(true))
}

//! The stream `context`, php's `php_stream_context` with its notifier
//! (`Z/main/streams/php_stream_context.h`, the `context` functions of
//! `Z/main/streams/streams.c`), owned by the stream layer beside
//! [`Stream`] as #1777 ruled: a `stream-context` resource holding the
//! options array and, where `notification` was set, the callable it
//! calls. `FG(default_context)` is a resource the request's list owns,
//! as php's is, named by an id in the layer's request state, since where
//! php keeps it is not observable. ext/standard's
//! `stream_context_*` natives and every other extension reach it here
//! through `php_ext`; a `transport` reads its `socket` options off it, and
//! a stream that keeps one tells its notifier what its reads and writes
//! came to, as php's streams do.

use super::{Request, SocketOptions};
use crate::api::call::call;
use crate::api::coerce::{callable_checked, to_long, truthy};
use crate::api::layer::Face as _;
use crate::api::raising::{Level, Thrown, docref, throw_error};
use crate::api::request::BindTo;
use crate::api::values::{
    Emalloc, Key, Owner, PhpArray, PhpString, Resource, ResourceView, Value, ValueRef,
};
use crate::state::Layer;

/// `sizeof(php_stream_context)` on the pinned 64-bit build: the
/// notifier's pointer, the `options` zval and the resource's pointer
/// (`Z/main/streams/php_stream_context.h:52-56`).
const CONTEXT_SIZE: usize = 32;

/// `sizeof(php_stream_notifier)` on the pinned 64-bit build: `func`,
/// `dtor`, `ptr`, `mask` and its padding, `progress` and `progress_max`
/// (`Z/main/streams/php_stream_context.h:44-50`).
const NOTIFIER_SIZE: usize = 48;

/// A `stream-context` resource, php's `php_stream_context` named by its
/// `res` (design §4.3 seam 5): what `stream_context_create` hands back,
/// what a stream keeps as `PHP_STREAM_CONTEXT(stream)`, and what a
/// wrapper's opener and a `transport` read their options off. Made only
/// by [`StreamCtx::alloc`], [`StreamCtx::default_ctx`] and
/// [`StreamCtx::fetch`], so a value of this type is a resource of
/// the `context`'s type and nothing else.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamCtx(Resource);

/// The payload of a `stream-context` resource: its options array and
/// its notifier, which php `ecalloc`s apart from it when `notification`
/// is set.
pub(crate) struct Context {
    options: Option<Owner<PhpArray>>,
    notifier: Option<Box<Notifier>>,
}

/// `php_stream_notifier` with php's one `func`,
/// `user_space_stream_notifier` (`Z/ext/standard/streamsfuncs.c:843`):
/// the callable, the `zend_fcall_info_cache` ext/standard resolved it
/// into, and the `progress` and `progress_max` it counts, which count
/// only once `php_stream_notify_progress_init` set
/// `PHP_STREAM_NOTIFIER_PROGRESS` in its `mask`; the notifier's own
/// `ecalloc` on the request's heap.
struct Notifier {
    callback: Owner<Value>,
    progress: usize,
    progress_max: usize,
    counting: bool,
    fcc: Emalloc,
    allocation: Emalloc,
}

impl Notifier {
    /// `php_stream_notification_free`: the `dtor`,
    /// `user_space_stream_notifier_dtor`, letting the callable go and
    /// `efree`ing its cache, then the notifier `efree`d.
    fn free(self, req: &mut Request) {
        self.callback.give_up(req);
        self.fcc.free(req);
        self.allocation.free(req);
    }
}

/// `PHP_STREAM_NOTIFY_*`, what a notification says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyCode {
    Connect = 2,
    AuthRequired = 3,
    MimeTypeIs = 4,
    FileSizeIs = 5,
    Redirected = 6,
    Progress = 7,
    Completed = 8,
    Failure = 9,
    AuthResult = 10,
}

/// One of the `php_stream_notify_*` macros of
/// `Z/main/streams/php_stream_context.h` a wrapper tells a `context`'s
/// notifier through ([`StreamCtx::notify`]); each message is the C
/// string php hands on, cut at its first NUL.
#[derive(Debug, Clone, Copy)]
pub enum Notification<'a> {
    /// `php_stream_notify_info(context, code, message, message_code)`.
    Info {
        code: NotifyCode,
        message: Option<&'a [u8]>,
        message_code: i64,
    },
    /// `php_stream_notify_error(context, code, message, message_code)`.
    Error {
        code: NotifyCode,
        message: Option<&'a [u8]>,
        message_code: i64,
    },
    /// `php_stream_notify_file_size(context, size, message, message_code)`.
    FileSize {
        size: usize,
        message: &'a [u8],
        message_code: i64,
    },
    /// `php_stream_notify_progress_init(context, sofar, max)`: the counts
    /// set, `PHP_STREAM_NOTIFIER_PROGRESS` in the `mask`, and the
    /// progress told.
    ProgressInit { sofar: usize, max: usize },
    /// `php_stream_notify_progress_increment(context, dsofar, dmax)`:
    /// where the notifier counts, both counts moved on and the progress
    /// told.
    ProgressIncrement { dsofar: usize, dmax: usize },
}

/// `PHP_STREAM_NOTIFY_SEVERITY_INFO` and `_ERR`, how it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Info = 0,
    Err = 2,
}

/// One call of `php_stream_notification_notify`.
struct Notice<'a> {
    code: NotifyCode,
    severity: Severity,
    message: Option<&'a [u8]>,
    message_code: i64,
    sofar: usize,
    max: usize,
}

/// `le_stream_context`'s destructor, `php_stream_context_free`: the
/// options and the notifier given back.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the resource destructor's shape, which a destructor that throws needs"
)]
pub(crate) fn context_dtor(req: &mut Request, res: Resource) -> Result<(), Thrown> {
    let taken = res
        .native_mut::<Context>(req)
        .map(|context| (context.options.take(), context.notifier.take()));
    if let Some((options, notifier)) = taken {
        if let Some(notifier) = notifier {
            notifier.free(req);
        }
        if let Some(options) = options {
            options.give_up(req);
        }
    }
    Ok(())
}

impl ResourceView for StreamCtx {
    fn resource(&self) -> Resource {
        self.0
    }
}

impl StreamCtx {
    /// `php_stream_context_alloc` (`Z/main/streams/streams.c:2422`): a
    /// `context` with an empty options array, a resource of its own with
    /// one owner, the caller's, in php's order: the struct, its
    /// `options`, then the resource, each at php's size. A request's
    /// first `fopen()` lays `FG(default_context)` down with its stream.
    ///
    /// ```php
    /// $m = memory_get_usage(); $f = fopen('/dev/null', 'r'); echo memory_get_usage() - $m, "\n"; fclose($f); unset($f);
    /// $c = stream_context_create(); unset($c);
    /// $m = memory_get_usage(); $c = stream_context_create(); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); stream_context_set_params($c, ['notification' => 'strlen']); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($c); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 616
    /// 120
    /// 88
    /// -208
    /// ```
    pub fn alloc(req: &mut Request) -> Result<Owner<StreamCtx>, Thrown> {
        let kind = req.types().context;
        let options = PhpArray::new(req)?;
        let context = Context {
            options: Some(options),
            notifier: None,
        };
        Ok(Resource::sized(req, kind, context, CONTEXT_SIZE)?.into_typed(StreamCtx))
    }

    /// `FG(default_context)`, made on first use and kept for the request:
    /// what `php_stream_context_from_zval(NULL, 0)` answers, which a
    /// native handed no `$context` opens under, as `gzopen` and
    /// `hash_update_file` do.
    ///
    /// The request's list owns it, as php's `EG(regular_list)` owns the
    /// resource `FG(default_context)` names by pointer, and closes it with
    /// the other resources; the layer keeps its id alone. Read after the
    /// list's close, it is the closed resource, where php's pointer names
    /// the `php_stream_context` the close freed.
    ///
    /// ```php
    /// var_dump(fopen('php://memory', 'r'), stream_context_get_default(), fopen('php://memory', 'r'));
    /// ```
    /// ```expect
    /// resource(5) of type (stream)
    /// resource(4) of type (stream-context)
    /// resource(6) of type (stream)
    /// ```
    pub fn default_ctx(req: &mut Request) -> Result<StreamCtx, Thrown> {
        if let Some(id) = req.streams().default_context
            && let Some(resource) = req.resource(id)
        {
            return Ok(StreamCtx(resource));
        }
        let context = StreamCtx::alloc(req)?;
        let view = *context;
        req.streams().default_context = Some(req.keep_in_list(context.into_resource()));
        Ok(view)
    }

    /// `zend_fetch_resource_ex(zcontext, "Stream-Context", le)` over
    /// `php_le_stream_context()`, the half of
    /// `php_stream_context_from_zval` that is handed a `$context`: the
    /// resource as a `context`, or php's `TypeError` naming the native
    /// being served where it is closed or of another type.
    ///
    /// ```php
    /// try { file_get_contents(__FILE__, false, STDIN); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { (new finfo)->file(__FILE__, 0, STDIN); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// file_get_contents(): supplied resource is not a valid Stream-Context resource
    /// finfo::file(): supplied resource is not a valid Stream-Context resource
    /// ```
    pub fn fetch(req: &mut Request, res: Resource) -> Result<StreamCtx, Thrown> {
        if let Some(context) = StreamCtx::from_resource(req, res) {
            return Ok(context);
        }
        let text = format!(
            "{}(): supplied resource is not a valid Stream-Context resource",
            req.active_function_name()
        );
        Err(throw_error(req, "TypeError", &text))
    }

    /// `zend_fetch_resource_ex(res, NULL, php_le_stream_context())`: the
    /// resource as a `context` where it is an open one, `None` for any
    /// other, raising nothing, as `decode_context_param` asks before it
    /// tries the resource as a stream.
    #[must_use]
    pub fn from_resource(req: &Request, res: Resource) -> Option<StreamCtx> {
        let kind = req.streams_ref()?.types?.context;
        (!res.is_closed() && res.kind() == Some(kind)).then_some(StreamCtx(res))
    }

    /// The `context` as the resource it is, `context->res`.
    #[must_use]
    pub const fn resource(self) -> Resource {
        self.0
    }

    /// `php_stream_context_get_option` (`Z/main/streams/streams.c:2440`):
    /// the value under `option` of the `wrapper`'s options, a view with
    /// no owner, dereferenced; `None` where either level is missing.
    #[must_use]
    pub fn option(self, req: &Request, wrapper: &[u8], option: &[u8]) -> Option<Value> {
        let options = self.options(req)?;
        let level = options.find(wrapper).map(|v| v.deref());
        let level = level.as_ref().and_then(Value::as_array)?;
        level.find(option).map(|v| v.deref())
    }

    /// The options array, `&context->options`, a view with no owner,
    /// which `stream_context_get_options` hands back with one more.
    #[must_use]
    pub fn options(self, req: &Request) -> Option<PhpArray> {
        self.0.native::<Context>(req)?.options.as_deref().copied()
    }

    /// `php_stream_context_set_option` (`Z/main/streams/streams.c:2457`):
    /// both levels separated, the wrapper's array made where it is
    /// missing, and the value, dereferenced and one owner more, written
    /// under the option's name; each name as the C string it is, up to
    /// its first NUL.
    pub fn set_option(
        self,
        req: &mut Request,
        wrapper: &[u8],
        option: &[u8],
        value: &Value,
    ) -> Result<(), Thrown> {
        let Some(mut options) = self.take_options(req) else {
            return Ok(());
        };
        let wrote = put_option(req, &mut options, wrapper, option, value);
        self.put_options(req, options);
        wrote
    }

    /// `php_stream_context_unset_option` (`Z/main/streams/streams.c:2477`):
    /// the value under `option` of the `wrapper`'s options taken out,
    /// both levels separated first; nothing where the `wrapper` has none.
    pub fn unset_option(
        self,
        req: &mut Request,
        wrapper: &[u8],
        option: &[u8],
    ) -> Result<(), Thrown> {
        let Some(mut options) = self.take_options(req) else {
            return Ok(());
        };
        let unset = unset_in(req, &mut options, wrapper, option);
        self.put_options(req, options);
        unset
    }

    /// The callable `context->notifier` calls, a view with no owner;
    /// `None` where the `context` has no notifier.
    #[must_use]
    pub fn notifier(self, req: &Request) -> Option<Value> {
        Some(*self.0.native::<Context>(req)?.notifier.as_ref()?.callback)
    }

    /// `context->notifier` replaced: the one there freed first,
    /// `php_stream_notification_free`, then, for `Some`,
    /// `php_stream_notification_alloc` over the callable, whose owner the
    /// caller hands over, and the `zend_fcall_info_cache` ext/standard's
    /// `parse_context_params` resolved it into, both given back with the
    /// notifier. The `Err` is the memory limit, both given back then.
    pub fn set_notifier(
        self,
        req: &mut Request,
        notifier: Option<(Owner<Value>, Emalloc)>,
    ) -> Result<(), Thrown> {
        let old = self
            .0
            .native_mut::<Context>(req)
            .and_then(|context| context.notifier.take());
        if let Some(old) = old {
            old.free(req);
        }
        let Some((callback, fcc)) = notifier else {
            return Ok(());
        };
        let allocation = match Emalloc::new(req, NOTIFIER_SIZE) {
            Ok(allocation) => allocation,
            Err(thrown) => {
                callback.give_up(req);
                fcc.free(req);
                return Err(thrown);
            }
        };
        let made = Box::new(Notifier {
            callback,
            progress: 0,
            progress_max: 0,
            counting: false,
            fcc,
            allocation,
        });
        match self.0.native_mut::<Context>(req) {
            Some(context) => context.notifier = Some(made),
            None => made.free(req),
        }
        Ok(())
    }

    /// One `php_stream_notify_*` told the `context`'s notifier; nothing
    /// where it has none, `if (context && context->notifier)`.
    pub fn notify(self, req: &mut Request, notification: Notification<'_>) -> Result<(), Thrown> {
        let notice = match notification {
            Notification::Info {
                code,
                message,
                message_code,
            } => Notice {
                code,
                severity: Severity::Info,
                message,
                message_code,
                sofar: 0,
                max: 0,
            },
            Notification::Error {
                code,
                message,
                message_code,
            } => Notice {
                code,
                severity: Severity::Err,
                message,
                message_code,
                sofar: 0,
                max: 0,
            },
            Notification::FileSize {
                size,
                message,
                message_code,
            } => Notice {
                code: NotifyCode::FileSizeIs,
                severity: Severity::Info,
                message: Some(message),
                message_code,
                sofar: 0,
                max: size,
            },
            Notification::ProgressInit { sofar, max } => {
                let Some(n) = self.notifier_mut(req) else {
                    return Ok(());
                };
                n.progress = sofar;
                n.progress_max = max;
                n.counting = true;
                Notice::progress(sofar, max)
            }
            Notification::ProgressIncrement { dsofar, dmax } => {
                let Some(n) = self.notifier_mut(req).filter(|n| n.counting) else {
                    return Ok(());
                };
                n.progress = n.progress.wrapping_add(dsofar);
                n.progress_max = n.progress_max.wrapping_add(dmax);
                Notice::progress(n.progress, n.progress_max)
            }
        };
        self.call_notifier(req, &notice)
    }

    /// `php_stream_notify_completed(context)`: the counts as they stand,
    /// what a stream's read or write that changed its `eof` tells. Any
    /// stream that keeps a `context` tells it, a socket client's among
    /// them, once, as the read that meets the end changes `eof`; nothing
    /// counts progress until a wrapper's `progress_init` asks for it.
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// $ctx = stream_context_create([], ['notification' => function (...$a) { echo json_encode($a), "\n"; }]);
    /// $c = stream_socket_client('tcp://' . stream_socket_get_name($s, false), $e, $m, 1, STREAM_CLIENT_CONNECT, $ctx);
    /// $a = stream_socket_accept($s);
    /// fwrite($a, "hello"); fclose($a);
    /// var_dump(stream_get_contents($c), fread($c, 1), feof($c));
    /// ```
    /// ```expect
    /// [8,0,null,0,0,0]
    /// string(5) "hello"
    /// string(0) ""
    /// bool(true)
    /// ```
    pub(crate) fn completed(self, req: &mut Request) -> Result<(), Thrown> {
        let Some(n) = self.notifier_mut(req) else {
            return Ok(());
        };
        let notice = Notice {
            code: NotifyCode::Completed,
            severity: Severity::Info,
            message: None,
            message_code: 0,
            sofar: n.progress,
            max: n.progress_max,
        };
        self.call_notifier(req, &notice)
    }

    /// The `socket` options of the `context`, each read as its reader in
    /// `xp_socket.c` and `transports.c` reads it: `bindto` a string or
    /// refused, `backlog` by `zval_get_long`, `ipv6_v6only` where it is
    /// not `null`, and the rest by `zend_is_true`. The `transport` reads
    /// them as it connects or binds ([`Request::xport_create`]), so every
    /// connection made under a `context` honours them, a `SoapClient`'s
    /// among them: with `bindto` naming an address, the server sees the
    /// request come from it.
    ///
    /// ```php
    /// ini_set('default_socket_timeout', 1);
    /// $t = stream_socket_server('tcp://127.0.0.1:0');
    /// $port = explode(':', stream_socket_get_name($t, false))[1];
    /// fclose($t);
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// $ctx = stream_context_create(['socket' => ['bindto' => "127.0.0.1:$port"]]);
    /// $c = new SoapClient(null, ['location' => 'http://' . stream_socket_get_name($s, false) . '/', 'uri' => 'urn:x', 'stream_context' => $ctx, 'exceptions' => false]);
    /// echo $c->__soapCall('f', [])->faultstring, "\n";
    /// stream_socket_accept($s, 1, $peer);
    /// var_dump($peer === "127.0.0.1:$port");
    /// ```
    /// ```expect
    /// Error Fetching http headers
    /// bool(true)
    /// ```
    pub(crate) fn socket_options(self, req: &mut Request) -> Result<SocketOptions, Thrown> {
        let mut options = SocketOptions::default();
        if let Some(value) = self.option(req, b"socket", b"bindto") {
            options.bindto = Some(match value.as_php_string() {
                Some(text) => BindTo::Address(text.as_bytes().to_vec()),
                None => BindTo::NotAString,
            });
        }
        if let Some(value) = self.option(req, b"socket", b"backlog") {
            options.listen_queue = Some(to_long(req, &value)?);
        }
        for (name, flag) in [
            (&b"tcp_nodelay"[..], &mut options.tcp_nodelay),
            (b"so_reuseport", &mut options.so_reuseport),
            (b"so_broadcast", &mut options.so_broadcast),
        ] {
            if let Some(value) = self.option(req, b"socket", name) {
                *flag = truthy(req, &value)?;
            }
        }
        if let Some(value) = self.option(req, b"socket", b"ipv6_v6only")
            && !matches!(value.as_ref(), ValueRef::Null)
        {
            options.ipv6_v6only = Some(truthy(req, &value)?);
        }
        Ok(options)
    }

    /// The options array taken out of the `context` for a call that
    /// changes it.
    fn take_options(self, req: &mut Request) -> Option<Owner<PhpArray>> {
        self.0.native_mut::<Context>(req)?.options.take()
    }

    /// The options array put back after [`StreamCtx::take_options`].
    fn put_options(self, req: &mut Request, options: Owner<PhpArray>) {
        match self.0.native_mut::<Context>(req) {
            Some(context) => context.options = Some(options),
            None => options.give_up(req),
        }
    }

    /// The `context`'s notifier, where it has one, to count into.
    fn notifier_mut(self, req: &mut Request) -> Option<&mut Notifier> {
        self.0.native_mut::<Context>(req)?.notifier.as_deref_mut()
    }

    /// `user_space_stream_notifier` (`Z/ext/standard/streamsfuncs.c:843`):
    /// the callable, held while it runs, given the six arguments, what it
    /// returns let go, and php's warning where it is no longer one to
    /// call; nothing where the `context` has no notifier.
    fn call_notifier(self, req: &mut Request, notice: &Notice<'_>) -> Result<(), Thrown> {
        let Some(callback) = self.notifier(req) else {
            return Ok(());
        };
        let held = Owner::take(req, callback);
        let called = call_user_notifier(req, &held, notice);
        held.give_up(req);
        called
    }
}

impl Notice<'_> {
    /// `php_stream_notify_progress(context, sofar, max)`.
    fn progress(sofar: usize, max: usize) -> Notice<'static> {
        Notice {
            code: NotifyCode::Progress,
            severity: Severity::Info,
            message: None,
            message_code: 0,
            sofar,
            max,
        }
    }
}

/// The call [`StreamCtx::call_notifier`] makes of the callback.
fn call_user_notifier(
    req: &mut Request,
    callback: &Value,
    notice: &Notice<'_>,
) -> Result<(), Thrown> {
    let Ok(callable) = callable_checked(req, callback) else {
        docref(req, Level::Warning, "Failed to call user notifier");
        return Ok(());
    };
    let message = match notice.message {
        Some(text) => {
            let text = crate::api::coerce::c_str(text);
            PhpString::new(req, text)?.into_value()
        }
        None => Owner::null(),
    };
    let long = |n: usize| Value::int(i64::try_from(n).unwrap_or(i64::MAX));
    let arguments = [
        Value::int(notice.code as i64),
        Value::int(notice.severity as i64),
        *message,
        Value::int(notice.message_code),
        long(notice.sofar),
        long(notice.max),
    ];
    let answered = call(req, &callable, &arguments);
    message.give_up(req);
    answered.map(|value| value.give_up(req))
}

/// The body of [`StreamCtx::set_option`] over the options array
/// taken out of the `context` for the call.
fn put_option(
    req: &mut Request,
    options: &mut Owner<PhpArray>,
    wrapper: &[u8],
    option: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    let c_string = |bytes: &[u8]| crate::api::coerce::c_str(bytes).to_vec();
    options.separate(req)?;
    let wrapper = c_string(wrapper);
    let existing = options.find(&wrapper).map(|v| v.deref());
    let wrapper_key = PhpString::new(req, &wrapper)?.into_key();
    let option_key = match PhpString::new(req, &c_string(option)) {
        Ok(option) => option.into_key(),
        Err(thrown) => {
            wrapper_key.give_up(req);
            return Err(thrown);
        }
    };
    let mut category = match existing.as_ref().and_then(Value::as_array) {
        Some(array) => Owner::take(req, array),
        None => match PhpArray::new(req) {
            Ok(array) => array,
            Err(thrown) => {
                wrapper_key.give_up(req);
                option_key.give_up(req);
                return Err(thrown);
            }
        },
    };
    let value = Owner::take(req, value.deref());
    if let Err(thrown) = category.set(req, option_key, value) {
        wrapper_key.give_up(req);
        category.give_up(req);
        return Err(thrown);
    }
    options.set(req, wrapper_key, category.into_value())
}

/// The body of [`StreamCtx::unset_option`] over the options array
/// taken out of the `context` for the call.
fn unset_in(
    req: &mut Request,
    options: &mut Owner<PhpArray>,
    wrapper: &[u8],
    option: &[u8],
) -> Result<(), Thrown> {
    let found = options
        .find(wrapper)
        .map(|v| v.deref())
        .and_then(|v| v.as_array());
    let Some(category) = found else {
        return Ok(());
    };
    let name = PhpString::new(req, wrapper)?.into_key();
    let mut category = Owner::take(req, category);
    let option = match PhpString::new(req, option) {
        Ok(option) => option,
        Err(thrown) => {
            name.give_up(req);
            category.give_up(req);
            return Err(thrown);
        }
    };
    let unset = category.unset(req, &Key::Str(*option));
    option.give_up(req);
    if let Err(thrown) = unset {
        name.give_up(req);
        category.give_up(req);
        return Err(thrown);
    }
    options.set(req, name, category.into_value())
}

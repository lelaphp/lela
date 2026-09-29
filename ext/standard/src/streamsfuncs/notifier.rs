//! A `context`'s notifier, `php_stream_notifier` with php's one kind of
//! it, `user_space_stream_notifier` over the callable `notification`
//! names (`Z/ext/standard/streamsfuncs.c:843`), and the
//! `php_stream_notify_*` family of `Z/main/streams/php_stream_context.h`
//! the http and ftp wrappers and the engine's sockets reach it through.

use php_ext::call::call;
use php_ext::coerce::callable_checked;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Notifier, Request, Stream};
use php_ext::values::{PhpString, Resource, Value};

use super::Context;

/// `php_stream_notifier` as `stream_context_set_params` makes it: the
/// callable, and the `progress` and `progress_max` it counts, which
/// count only once `php_stream_notify_progress_init` set
/// `PHP_STREAM_NOTIFIER_PROGRESS` in its `mask`.
pub(super) struct Notification {
    pub(super) callback: Value,
    progress: usize,
    progress_max: usize,
    counting: bool,
}

impl Notification {
    /// `php_stream_notification_alloc` with the callable, one owner the
    /// notifier's: nothing counted and no bit in the `mask`.
    pub(super) const fn new(callback: Value) -> Notification {
        Notification {
            callback,
            progress: 0,
            progress_max: 0,
            counting: false,
        }
    }
}

/// `PHP_STREAM_NOTIFY_*`, what a notification says happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
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

/// `PHP_STREAM_NOTIFY_SEVERITY_INFO` and `_ERR`, how it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Info = 0,
    Err = 2,
}

/// One call of `php_stream_notification_notify`: `code` at `severity`,
/// the message as the C string php hands on, cut at its first NUL, the
/// message's code and the two byte counts.
struct Notice<'a> {
    code: Code,
    severity: Severity,
    message: Option<&'a [u8]>,
    message_code: i64,
    sofar: usize,
    max: usize,
}

/// The `context`'s notifier, where it has one, read.
fn notification(req: &Request, ctx: Resource) -> Option<&Notification> {
    ctx.native::<Context>(req)?.notifier.as_ref()
}

/// The `context`'s notifier, where it has one, to count into.
fn notification_mut(req: &mut Request, ctx: Resource) -> Option<&mut Notification> {
    ctx.native_mut::<Context>(req)?.notifier.as_mut()
}

/// `user_space_stream_notifier` (`Z/ext/standard/streamsfuncs.c:843`):
/// the callable given the six arguments, what it returns let go, and
/// php's warning where it is no longer one to call. Nothing where the
/// `context` has no notifier, `if (context && context->notifier)`.
fn notify(req: &mut Request, ctx: Resource, notice: &Notice<'_>) -> Result<(), Thrown> {
    let Some(callback) = notification(req, ctx).map(|n| n.callback) else {
        return Ok(());
    };
    req.addref(&callback);
    let called = call_notifier(req, &callback, notice);
    req.release(callback);
    called
}

/// The call [`notify`] makes of the callback, held while it runs.
fn call_notifier(req: &mut Request, callback: &Value, notice: &Notice<'_>) -> Result<(), Thrown> {
    let Ok(callable) = callable_checked(req, callback) else {
        docref(req, Level::Warning, "Failed to call user notifier");
        return Ok(());
    };
    let message = match notice.message {
        Some(text) => {
            let text = text.split(|b| *b == 0).next().unwrap_or_default();
            Value::string(PhpString::new(req, text)?)
        }
        None => Value::null(),
    };
    let long = |n: usize| Value::int(i64::try_from(n).unwrap_or(i64::MAX));
    let arguments = [
        Value::int(notice.code as i64),
        Value::int(notice.severity as i64),
        message,
        Value::int(notice.message_code),
        long(notice.sofar),
        long(notice.max),
    ];
    let answered = call(req, &callable, &arguments);
    req.release(message);
    answered.map(|value| req.release(value))
}

/// `php_stream_notify_info(context, code, message, message_code)`.
pub(crate) fn info(
    req: &mut Request,
    ctx: Option<Resource>,
    code: Code,
    message: Option<&[u8]>,
    message_code: i64,
) -> Result<(), Thrown> {
    let Some(ctx) = ctx else { return Ok(()) };
    let notice = Notice {
        code,
        severity: Severity::Info,
        message,
        message_code,
        sofar: 0,
        max: 0,
    };
    notify(req, ctx, &notice)
}

/// `php_stream_notify_error(context, code, message, message_code)`.
pub(crate) fn error(
    req: &mut Request,
    ctx: Option<Resource>,
    code: Code,
    message: Option<&[u8]>,
    message_code: i64,
) -> Result<(), Thrown> {
    let Some(ctx) = ctx else { return Ok(()) };
    let notice = Notice {
        code,
        severity: Severity::Err,
        message,
        message_code,
        sofar: 0,
        max: 0,
    };
    notify(req, ctx, &notice)
}

/// `php_stream_notify_file_size(context, size, message, message_code)`.
pub(crate) fn file_size(
    req: &mut Request,
    ctx: Option<Resource>,
    size: usize,
    message: &[u8],
    message_code: i64,
) -> Result<(), Thrown> {
    let Some(ctx) = ctx else { return Ok(()) };
    let notice = Notice {
        code: Code::FileSizeIs,
        severity: Severity::Info,
        message: Some(message),
        message_code,
        sofar: 0,
        max: size,
    };
    notify(req, ctx, &notice)
}

/// `php_stream_notify_progress(context, sofar, max)`.
fn progress(req: &mut Request, ctx: Resource, sofar: usize, max: usize) -> Result<(), Thrown> {
    let notice = Notice {
        code: Code::Progress,
        severity: Severity::Info,
        message: None,
        message_code: 0,
        sofar,
        max,
    };
    notify(req, ctx, &notice)
}

/// `php_stream_notify_progress_init(context, sofar, max)`: the counts
/// set, `PHP_STREAM_NOTIFIER_PROGRESS` in the `mask`, and the progress
/// told.
pub(crate) fn progress_init(
    req: &mut Request,
    ctx: Option<Resource>,
    sofar: usize,
    max: usize,
) -> Result<(), Thrown> {
    let Some(ctx) = ctx else { return Ok(()) };
    let Some(n) = notification_mut(req, ctx) else {
        return Ok(());
    };
    n.progress = sofar;
    n.progress_max = max;
    n.counting = true;
    progress(req, ctx, sofar, max)
}

/// `php_stream_notify_progress_increment(context, dsofar, dmax)`: where
/// the notifier counts, both counts moved on and the progress told.
pub(crate) fn progress_increment(
    req: &mut Request,
    ctx: Option<Resource>,
    dsofar: usize,
    dmax: usize,
) -> Result<(), Thrown> {
    let Some(ctx) = ctx else { return Ok(()) };
    let Some(n) = notification_mut(req, ctx).filter(|n| n.counting) else {
        return Ok(());
    };
    n.progress = n.progress.wrapping_add(dsofar);
    n.progress_max = n.progress_max.wrapping_add(dmax);
    let (sofar, max) = (n.progress, n.progress_max);
    progress(req, ctx, sofar, max)
}

/// `php_stream_notify_completed(context)`: the counts as they stand.
fn completed(req: &mut Request, ctx: Resource) -> Result<(), Thrown> {
    let Some((sofar, max)) = notification(req, ctx).map(|n| (n.progress, n.progress_max)) else {
        return Ok(());
    };
    let notice = Notice {
        code: Code::Completed,
        severity: Severity::Info,
        message: None,
        message_code: 0,
        sofar,
        max,
    };
    notify(req, ctx, &notice)
}

/// `PHP_STREAM_CONTEXT(stream)` as the engine reaches it: the `context`
/// resource, one owner held while the stream lives.
struct Held(Resource);

impl Notifier for Held {
    fn progress(&mut self, req: &mut Request, bytes: usize) -> Result<(), Thrown> {
        progress_increment(req, Some(self.0), bytes, 0)
    }

    fn completed(&mut self, req: &mut Request) -> Result<(), Thrown> {
        completed(req, self.0)
    }

    fn release(self: Box<Self>, req: &mut Request) {
        req.release(self.0.as_value());
    }
}

/// `php_stream_context_set(stream, context)`: the stream's reads and
/// writes reach the `context`'s notifier from here, whichever it has
/// then, as php's stream holds the `context` itself.
pub(crate) fn attach(req: &mut Request, stream: Stream, ctx: Option<Resource>) {
    let Some(ctx) = ctx else { return };
    req.addref(&ctx.as_value());
    stream.set_notifier(req, Box::new(Held(ctx)));
}

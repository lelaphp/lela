//! The `CurlHandle` class and its payload, php's `php_curl`
//! (`Z/ext/curl/curl_private.h:95`): the `CURL *`, where each of
//! the three transfer streams goes, the callables, what the last transfer
//! left and the values the `CurlHandle` holds; with `curl_object_handlers`
//! and the natives of `interface.c` that act on one `CurlHandle`.

use php_ext::abi::{
    Callable, CloneObj, GcTable, Instance, ObjectHandlers, OrFalse, Refusals, Union,
};
use php_ext::attr::{php_class, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::raising::{Level, Thrown, clear, docref, throw, throw_error};
use php_ext::request::{Request, Stream};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, SmartStr, TypeTag, Value};

use crate::State;
use crate::ffi::{Easy, EasyId, Hook};
use crate::throwables::{Pending, Throwables};

/// A callable kept past the call it was given to, `zend_fcc_dup`
/// (`Z/ext/curl/interface.c:1224`, `multi.c:493`): its value's owner and
/// the receiver's it resolved to; given up as `zend_fcc_dtor` gives it.
pub(crate) type Handler = Owner<Callable>;

/// Another owner of what `handler` keeps, `zend_fcc_dup` of a copy.
pub(crate) fn dup(req: &Request, handler: &Handler) -> Handler {
    Owner::take(req, Callable::clone(handler))
}

/// `ch->handlers.write->method`: where the body goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum WriteTo {
    /// `PHP_CURL_STDOUT`, written as `echo` writes.
    #[default]
    Stdout,
    /// `PHP_CURL_FILE`, the `CURLOPT_FILE` stream.
    File,
    /// `PHP_CURL_RETURN`, kept for `curl_exec` to return.
    Return,
    /// `PHP_CURL_USER`, the `CURLOPT_WRITEFUNCTION` callable.
    User,
}

/// `ch->handlers.write_header->method`: where the headers go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HeaderTo {
    /// `PHP_CURL_IGNORE`.
    #[default]
    Ignore,
    /// `PHP_CURL_FILE`, the `CURLOPT_WRITEHEADER` stream.
    File,
    /// `PHP_CURL_USER`, the `CURLOPT_HEADERFUNCTION` callable.
    User,
}

/// `ch->handlers.read->method`: where an upload comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ReadFrom {
    /// `PHP_CURL_DIRECT`, the `CURLOPT_INFILE` stream where one is set.
    #[default]
    Direct,
    /// `PHP_CURL_USER`, the `CURLOPT_READFUNCTION` callable.
    User,
}

/// `php_curl_write` and `php_curl_read`: the method, the callable and
/// the stream, each kept whatever the method.
#[derive(Default)]
pub(crate) struct Channel<M> {
    pub(crate) to: M,
    pub(crate) handler: Option<Handler>,
    pub(crate) stream: Option<Owner<Value>>,
}

impl<M: Copy> Channel<M> {
    fn dup(&self, req: &Request) -> Channel<M> {
        Channel {
            to: self.to,
            handler: self.handler.as_ref().map(|handler| dup(req, handler)),
            stream: self
                .stream
                .as_deref()
                .map(|stream| Owner::take(req, *stream)),
        }
    }

    /// The stream, a view.
    pub(crate) fn stream(&self) -> Option<Value> {
        self.stream.as_deref().copied()
    }

    fn release(self, req: &mut Request) {
        if let Some(handler) = self.handler {
            handler.give_up(req);
        }
        if let Some(stream) = self.stream {
            stream.give_up(req);
        }
    }
}

/// `handlers.write->buf`, the body `PHP_CURL_RETURN` keeps: a
/// `smart_str` on the request's heap, grown as the body is written, and
/// once `curl_exec` or `curl_multi_getcontent` handed its string out
/// (`RETURN_STR_COPY`), that string, of which the `CurlHandle` keeps one owner
/// until `smart_str_free` gives it up.
pub(crate) enum Body {
    /// `buf.s` as `smart_str_appendl` grows it, nothing allocated while
    /// nothing was written.
    Growing(SmartStr),
    /// `buf.s` with a second owner handed out.
    Handed(Owner<PhpString>),
}

/// `curl_write`'s `PHP_CURL_RETURN`: `data` after the body `object` kept,
/// `smart_str_fappendl_ex(..., fallible: true)` since 8.5.12, and `false`
/// where the memory limit refused the room, which the transfer is told
/// as `CURL_WRITEFUNC_ERROR`. A refused growth leaves its block to the
/// request's end, the body kept from there being empty.
pub(crate) fn keep(req: &mut Request, object: PhpObject, data: &[u8]) -> Result<bool, Thrown> {
    let Some(curl) = object.native_mut::<Curl>(req) else {
        return Ok(true);
    };
    let (mut buf, handed) = match std::mem::take(&mut curl.returned) {
        Body::Growing(buf) => (buf, None),
        Body::Handed(string) => (SmartStr::default(), Some(string)),
    };
    let appended = req.memory_mut().fallible(|req| {
        if let Some(string) = &handed {
            buf.append(req, string.as_bytes())?;
        }
        buf.append(req, data)
    });
    if let Some(string) = handed {
        string.give_up(req);
    }
    let appended = appended?.is_some();
    if !appended {
        buf = SmartStr::default();
    }
    match object.native_mut::<Curl>(req) {
        Some(curl) => curl.returned = Body::Growing(buf),
        None => buf.free(req),
    }
    Ok(appended)
}

/// `smart_str_0(&buf)` and `RETURN_STR_COPY(buf.s)`: the body `object`
/// kept, the `CurlHandle` keeping one owner of it; the empty string where
/// nothing was written (`!buf.s`).
pub(crate) fn hand_out(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let Some(curl) = object.native_mut::<Curl>(req) else {
        return PhpString::new(req, b"");
    };
    let string = match std::mem::take(&mut curl.returned) {
        Body::Growing(buf) if buf.as_bytes().is_empty() => return PhpString::new(req, b""),
        Body::Growing(buf) => buf.finish(req)?,
        Body::Handed(string) => string,
    };
    let out = Owner::take(req, *string);
    match object.native_mut::<Curl>(req) {
        Some(curl) => curl.returned = Body::Handed(string),
        None => string.give_up(req),
    }
    Ok(out)
}

/// `smart_str_free(&ch->handlers.write->buf)`.
pub(crate) fn free_body(req: &mut Request, object: PhpObject) {
    if let Some(curl) = object.native_mut::<Curl>(req) {
        let body = std::mem::take(&mut curl.returned);
        body.free(req);
    }
}

impl Default for Body {
    fn default() -> Body {
        Body::Growing(SmartStr::default())
    }
}

impl Body {
    fn free(self, req: &mut Request) {
        match self {
            Body::Growing(mut buf) => buf.free(req),
            Body::Handed(string) => string.give_up(req),
        }
    }
}

/// `php_curl` less its `zend_object`.
#[derive(Default)]
pub(crate) struct Curl {
    /// `cp`: `None` until `curl_init` lays it, and where a copy failed;
    /// the request keeps the `CURL *` and this is its one owner.
    pub(crate) easy: Option<Easy>,
    pub(crate) write: Channel<WriteTo>,
    pub(crate) header: Channel<HeaderTo>,
    pub(crate) read: Channel<ReadFrom>,
    /// `handlers.write->buf`, what `PHP_CURL_RETURN` kept.
    pub(crate) returned: Body,
    /// `handlers.std_err`, the `CURLOPT_STDERR` stream.
    pub(crate) stderr: Option<Owner<Value>>,
    /// The six callables [`Hook`] names.
    hooks: [Option<Handler>; 6],
    /// `header.str`, the last request's headers as `CURLINFO_HEADER_OUT`
    /// kept them.
    pub(crate) header_out: Option<Vec<u8>>,
    /// `err.no`.
    pub(crate) err: i64,
    pub(crate) in_callback: bool,
    /// The `CURLOPT_POSTFIELDS` array that held a file, rebuilt by a copy.
    pub(crate) postfields: Option<Owner<PhpArray>>,
    /// `CURLOPT_PRIVATE`.
    pub(crate) private: Option<Owner<Value>>,
    /// The `CurlShareHandle` of `CURLOPT_SHARE`, an owner of it.
    pub(crate) share: Option<Owner<PhpObject>>,
}

const fn slot(hook: Hook) -> usize {
    match hook {
        Hook::Progress => 0,
        Hook::Xferinfo => 1,
        Hook::Fnmatch => 2,
        Hook::Debug => 3,
        Hook::Prereq => 4,
        Hook::SshHostkey => 5,
    }
}

impl Curl {
    pub(crate) fn hook(&self, hook: Hook) -> Option<&Handler> {
        self.hooks[slot(hook)].as_ref()
    }

    pub(crate) fn hook_mut(&mut self, hook: Hook) -> &mut Option<Handler> {
        &mut self.hooks[slot(hook)]
    }

    /// Every value the `CurlHandle` holds, let go: what `curl_free_obj` and
    /// `_php_curl_reset_handlers` release, the `CURL *` aside.
    pub(crate) fn release_values(self, req: &mut Request) {
        self.write.release(req);
        self.header.release(req);
        self.read.release(req);
        for handler in self.hooks.into_iter().flatten() {
            handler.give_up(req);
        }
        for value in [self.stderr, self.private].into_iter().flatten() {
            value.give_up(req);
        }
        if let Some(postfields) = self.postfields {
            postfields.give_up(req);
        }
        if let Some(share) = self.share {
            share.give_up(req);
        }
        self.returned.free(req);
    }
}

/// `final class CurlHandle`, `@strict-properties`, `@not-serializable`.
///
/// ```php
/// try { new CurlHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $c = curl_init();
/// try { serialize($c); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { $c->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($c, (int) $c, $c == curl_init(), clone $c);
/// ```
/// ```expect
/// Cannot directly construct CurlHandle, use curl_init() instead
/// Serialization of 'CurlHandle' is not allowed
/// Cannot create dynamic property CurlHandle::$x
/// object(CurlHandle)#1 (0) {
/// }
/// int(1)
/// bool(false)
/// object(CurlHandle)#3 (0) {
/// }
/// ```
#[php_class(
    final,
    not_serializable,
    payload = Curl,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct CurlHandle;

#[php_class]
impl CurlHandle {}

/// `curl_object_handlers`: construction and comparison refused, and
/// cloning served by `curl_clone_obj`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What `curl_object_handlers` refuses: construction and comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("curl_init()"))
    .comparison();

impl ObjectHandlers for Handlers {
    /// `curl_free_obj`: the streams verified silently
    /// ([`verify_before_cleanup`]), the `CURL *` cleaned up with the
    /// request reachable, or as the transfer holding it ends, then
    /// everything the `CurlHandle` held let go, the share's object last,
    /// as php releases `ch->share` after `curl_easy_cleanup`; what a
    /// callback threw is left pending, as php leaves it in
    /// `EG(exception)`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let mut pending = Pending::default();
        verify_before_cleanup(req, object, &mut pending);
        let mut curl = object.native_mut::<Curl>(req).map(std::mem::take);
        let easy = curl.as_mut().and_then(|curl| curl.easy.take());
        if let Some(thrown) = easy.and_then(|easy| easy.close(req)) {
            pending.park(req, thrown);
        }
        pending.release(req, |req| {
            if let Some(curl) = curl.take() {
                curl.release_values(req);
            }
        });
        pending.leave(req);
    }

    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(copied)
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(curl) = object.native::<Curl>(req) else {
            return GcTable::Properties;
        };
        let channels = [
            (&curl.read.handler, &curl.read.stream),
            (&curl.write.handler, &curl.write.stream),
            (&curl.header.handler, &curl.header.stream),
        ];
        for (handler, stream) in channels {
            if let Some(handler) = handler {
                handler.visit(visit);
            }
            if let Some(stream) = stream {
                visit(**stream);
            }
        }
        for handler in curl.hooks.iter().flatten() {
            handler.visit(visit);
        }
        if let Some(postfields) = &curl.postfields {
            visit(Value::array(**postfields));
        }
        for value in [&curl.stderr, &curl.private].into_iter().flatten() {
            visit(**value);
        }
        GcTable::Properties
    }

    /// `curl_cast_object`: `(int)` is the object's id, as the resource's
    /// was.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        cast_object(req, object, to)
    }
}

/// `curl_cast_object`, which the `CURLM *` shares: `(int)` the
/// object's id, and `zend_std_cast_object_tostring` otherwise, which is
/// `true` for a bool and nothing for a class with no `__toString`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `cast_object` slot's shape, which the handlers hand on"
)]
pub(crate) fn cast_object(
    req: &mut Request,
    object: PhpObject,
    to: TypeTag,
) -> Result<Option<Owner<Value>>, Thrown> {
    let cast = match to {
        TypeTag::Int => Value::int(i64::from(object.index())),
        TypeTag::Bool => Value::bool(true),
        _ => return Ok(None),
    };
    Ok(Some(Owner::take(req, cast)))
}

/// A new object of a class this extension declares.
pub(crate) fn made(req: &mut Request, class: &[u8]) -> Result<Owner<PhpObject>, Thrown> {
    let Some(id) = lookup_class(req, class)? else {
        let text = format!("Class \"{}\" not found", String::from_utf8_lossy(class));
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, &text));
    };
    new_object(req, id)
}

/// The payload of a `CurlHandle` argument.
pub(crate) fn curl(req: &mut Request, handle: Instance<CurlHandle>) -> Result<&mut Curl, Thrown> {
    handle.payload_mut(req)
}

/// The `CURL *` of a `CurlHandle` argument, by name.
pub(crate) fn easy_of(req: &mut Request, object: PhpObject) -> Result<EasyId, Thrown> {
    let curl = object.native_mut_or_fatal::<Curl>(req)?;
    match &curl.easy {
        Some(easy) => Ok(easy.id()),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a CurlHandle with no easy handle",
        )),
    }
}

/// `curl_clone_obj`: a new `CurlHandle` over `curl_easy_duphandle`, the
/// callables, streams and private value copied, and a file upload's
/// MIME tree built again for the copy, its owner the caller's; what
/// `curl_copy_handle` returns too.
fn copied(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let copy = made(req, b"CurlHandle")?;
    let failed = |req: &mut Request, copy: Owner<PhpObject>| {
        copy.give_up(req);
        match Throwables::of(req) {
            Ok(ce) => throw(req, ce.exception, "Failed to clone CurlHandle"),
            Err(fatal) => fatal,
        }
    };
    let source = match easy_of(req, object) {
        Ok(source) => source,
        Err(thrown) => {
            copy.give_up(req);
            return Err(thrown);
        }
    };
    let Some(easy) = source.duplicate(req, *copy) else {
        return Err(failed(req, copy));
    };
    let dup = match copy_handlers(req, object) {
        Ok(dup) => dup,
        Err(thrown) => {
            let _ = easy.close(req);
            copy.give_up(req);
            return Err(thrown);
        }
    };
    let postfields = dup.postfields.as_deref().copied();
    if let Some(curl) = copy.native_mut::<Curl>(req) {
        *curl = dup;
        curl.easy = Some(easy);
    } else {
        let _ = easy.close(req);
        dup.release_values(req);
    }
    if let Some(array) = postfields {
        let mut pending = Pending::default();
        let built = crate::file::build_mime(req, *copy, array, &mut pending);
        if pending.answer(req, built).is_err() {
            return Err(failed(req, copy));
        }
    }
    Ok(copy)
}

/// `_php_setup_easy_copy_handlers`: the three channels, the six
/// callables and the private value, each with one more owner; and the
/// `CURLOPT_STDERR` stream, which php's copy does not hold though its
/// `CURL *` writes to the stream's `FILE *`, held, so the copy's file is
/// never left over a stream the original let go of.
pub(crate) fn copy_handlers(req: &mut Request, object: PhpObject) -> Result<Curl, Thrown> {
    object.native_mut_or_fatal::<Curl>(req)?;
    let mut copy = Curl {
        write: channel(req, object, |c| &c.write),
        header: channel(req, object, |c| &c.header),
        read: channel(req, object, |c| &c.read),
        ..Curl::default()
    };
    let hooks: Vec<Option<Handler>> = match object.native::<Curl>(req) {
        Some(from) => from
            .hooks
            .iter()
            .map(|hook| hook.as_ref().map(|handler| dup(req, handler)))
            .collect(),
        None => Vec::new(),
    };
    for (slot, hook) in copy.hooks.iter_mut().zip(hooks) {
        *slot = hook;
    }
    let (private, postfields, stderr) = match object.native::<Curl>(req) {
        Some(from) => (
            from.private.as_deref().copied(),
            from.postfields.as_deref().copied(),
            from.stderr.as_deref().copied(),
        ),
        None => (None, None, None),
    };
    copy.private = private.map(|private| Owner::take(req, private));
    copy.stderr = stderr.map(|stderr| Owner::take(req, stderr));
    copy.postfields = postfields.map(|postfields| Owner::take(req, postfields));
    Ok(copy)
}

/// One channel of `object`, copied.
fn channel<M: Copy + Default>(
    req: &Request,
    object: PhpObject,
    pick: impl Fn(&Curl) -> &Channel<M>,
) -> Channel<M> {
    match object.native::<Curl>(req) {
        Some(from) => pick(from).dup(req),
        None => Channel::default(),
    }
}

/// `_php_curl_set_default_options` over `easy`, with the CA file
/// `openssl.cafile` or `curl.cainfo` names; what a callback threw
/// during the sets.
pub(crate) fn set_defaults(req: &mut Request, easy: EasyId) -> Option<Thrown> {
    let cainfo = [b"openssl.cafile".as_slice(), crate::CAINFO]
        .into_iter()
        .find_map(|name| req.ini().get(name).filter(|value| !value.is_empty()))
        .map(<[u8]>::to_vec);
    easy.defaults(req, cainfo.as_deref())
}

/// `_php_curl_verify_handlers(ch, reporterror)`: a stream the `CurlHandle`
/// names that was closed meanwhile let go, the channel reset to its
/// default, with php's warning where `report`; what a callback threw
/// as `CURLOPT_STDERR` was set back.
pub(crate) fn verify_handlers(
    req: &mut Request,
    object: PhpObject,
    report: bool,
) -> Option<Thrown> {
    let curl = object.native::<Curl>(req)?;
    let gone = |stream: &Option<Owner<Value>>| {
        stream
            .as_deref()
            .is_some_and(|stream| Stream::from_value(req, stream).is_none())
    };
    let stale = [
        gone(&curl.stderr),
        gone(&curl.read.stream),
        gone(&curl.header.stream),
        gone(&curl.write.stream),
    ];
    let easy = curl.easy.as_ref().map(Easy::id);
    let messages = [
        "CURLOPT_STDERR resource has gone away, resetting to stderr",
        "CURLOPT_INFILE resource has gone away, resetting to default",
        "CURLOPT_WRITEHEADER resource has gone away, resetting to default",
        "CURLOPT_FILE resource has gone away, resetting to default",
    ];
    let mut thrown = None;
    for (which, (stale, message)) in stale.into_iter().zip(messages).enumerate() {
        if !stale {
            continue;
        }
        if report {
            docref(req, Level::Warning, message);
        }
        let Some(curl) = object.native_mut::<Curl>(req) else {
            return thrown;
        };
        let released = match which {
            0 => curl.stderr.take(),
            1 => curl.read.stream.take(),
            2 => {
                curl.header.to = HeaderTo::Ignore;
                curl.header.stream.take()
            }
            _ => {
                curl.write.to = WriteTo::Stdout;
                curl.write.stream.take()
            }
        };
        match (released, easy) {
            (Some(value), Some(easy)) if which == 0 => {
                (_, thrown) = easy.set_stderr(req, None);
                easy.let_stream_go(req, value);
            }
            (Some(value), _) => value.give_up(req),
            (None, _) => {}
        }
    }
    thrown
}

/// `_php_curl_verify_handlers(ch, /* reporterror */ false)`, as
/// `curl_free_obj` and `curl_multi_free_obj` call it before the cleanup
/// that may write a last line: a stream closed meanwhile let go and its
/// channel reset without a warning, so the line reaches the process's
/// `stderr` and not a stream gone. Taken as a step under what `pending`
/// holds, which what a callback threw joins. A `CurlHandle` whose free is
/// under way, php's `IS_OBJ_FREE_CALLED`, has had its payload taken by
/// its `free_obj` or dropped after it, so it names no stream and is
/// passed over, as php's walk passes it.
pub(crate) fn verify_before_cleanup(req: &mut Request, object: PhpObject, pending: &mut Pending) {
    pending.step(req, |req, pending| {
        if let Some(thrown) = verify_handlers(req, object, false) {
            pending.park(req, thrown);
        }
    });
}

/// `_php_curl_cleanup_handle`: what the last transfer left, cleared.
pub(crate) fn cleanup_handle(req: &mut Request, object: PhpObject) {
    let Some(curl) = object.native_mut::<Curl>(req) else {
        return;
    };
    let body = std::mem::take(&mut curl.returned);
    curl.header_out = None;
    curl.err = 0;
    if let Some(easy) = curl.easy.as_ref().map(Easy::id) {
        easy.clear_error(req);
    }
    body.free(req);
}

/// `curl_init(?string $url = null): CurlHandle|false`.
///
/// ```php
/// $c = curl_init('file:///nonexistent-curl-witness');
/// var_dump($c instanceof CurlHandle, curl_getinfo($c, CURLINFO_EFFECTIVE_URL));
/// try { curl_init("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(32) "file:///nonexistent-curl-witness"
/// curl_init(): cURL option must not contain any null bytes
/// ```
#[php_function]
fn curl_init(
    req: &mut Request,
    #[php(default = null)] url: Option<PhpString>,
) -> Result<OrFalse<Owner<Instance<CurlHandle>>>, Thrown> {
    let object = made(req, b"CurlHandle")?;
    let session = req.state::<State>().session.clone();
    let Some(easy) = Easy::init(req, session, *object) else {
        object.give_up(req);
        docref(
            req,
            Level::Warning,
            "Could not initialize a new cURL handle",
        );
        return Ok(OrFalse::False);
    };
    let defaulted = set_defaults(req, easy.id());
    match object.native_mut::<Curl>(req) {
        Some(curl) => curl.easy = Some(easy),
        None => drop(easy.close(req)),
    }
    if let Some(thrown) = defaulted {
        object.give_up(req);
        return Err(thrown);
    }
    if let Some(url) = url {
        let url = url.as_bytes().to_vec();
        let mut pending = Pending::default();
        let answered = crate::options::set_url(req, *object, &url, &mut pending);
        match pending.answer(req, answered) {
            Ok(true) => {}
            Ok(false) => {
                object.give_up(req);
                return Ok(OrFalse::False);
            }
            Err(thrown) => {
                object.give_up(req);
                return Err(thrown);
            }
        }
    }
    Ok(OrFalse::Value(object.into_instance()))
}

/// `curl_copy_handle(CurlHandle $handle): CurlHandle|false`.
///
/// ```php
/// $c = curl_init('file:///a');
/// curl_setopt($c, CURLOPT_PRIVATE, 'kept');
/// $d = curl_copy_handle($c);
/// var_dump($d === $c, curl_getinfo($d, CURLINFO_PRIVATE), curl_getinfo($d, CURLINFO_EFFECTIVE_URL));
/// ```
/// ```expect
/// bool(false)
/// string(4) "kept"
/// string(9) "file:///a"
/// ```
#[php_function]
fn curl_copy_handle(
    req: &mut Request,
    handle: Instance<CurlHandle>,
) -> Result<OrFalse<Owner<Instance<CurlHandle>>>, Thrown> {
    match copied(req, handle.object()) {
        Ok(copy) => Ok(OrFalse::Value(copy.into_instance())),
        Err(thrown) => {
            clear(req, thrown)?;
            docref(req, Level::Warning, "Cannot duplicate cURL handle");
            Ok(OrFalse::False)
        }
    }
}

/// `curl_errno(CurlHandle $handle): int`.
///
/// ```php
/// $c = curl_init('file:///nonexistent-curl-witness');
/// var_dump(curl_errno($c));
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// curl_exec($c);
/// var_dump(curl_errno($c));
/// ```
/// ```expect
/// int(0)
/// int(37)
/// ```
#[php_function]
fn curl_errno(req: &mut Request, handle: Instance<CurlHandle>) -> Result<i64, Thrown> {
    Ok(curl(req, handle)?.err)
}

/// `curl_error(CurlHandle $handle): string`: the error buffer's text, or
/// the code's own where the buffer is empty.
///
/// ```php
/// $c = curl_init('file:///nonexistent-curl-witness');
/// var_dump(curl_error($c));
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// curl_exec($c);
/// var_dump(curl_error($c));
/// ```
/// ```expect
/// string(0) ""
/// string(%d) "Could%sopen file %snonexistent-curl-witness%S"
/// ```
#[php_function]
fn curl_error(req: &mut Request, handle: Instance<CurlHandle>) -> Result<Owner<PhpString>, Thrown> {
    let curl = curl(req, handle)?;
    if curl.err == 0 {
        return PhpString::new(req, b"");
    }
    let err = curl.err;
    let easy = curl.easy.as_ref().map(Easy::id);
    let text = easy.map(|easy| easy.error_text(req)).unwrap_or_default();
    let text = if text.is_empty() {
        crate::ffi::strerror(err).unwrap_or_default()
    } else {
        text
    };
    PhpString::new(req, &text)
}

/// `curl_close(CurlHandle $handle): void`, which does nothing since the
/// `CurlHandle` became an object but refuse a callback's call.
///
/// ```php
/// curl_close(curl_init());
/// ```
/// ```expect
/// Deprecated: Function curl_close() is deprecated since 8.5, as it has no effect since PHP 8.0 in %s on line 2
/// ```
#[php_function(deprecated(since = "8.5", message = "as it has no effect since PHP 8.0"))]
fn curl_close(req: &mut Request, handle: Instance<CurlHandle>) -> Result<(), Thrown> {
    if curl(req, handle)?.in_callback {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "curl_close(): Attempt to close cURL handle from a callback",
        ));
    }
    Ok(())
}

/// `_php_curl_reset_handlers`: every channel back to its default, its
/// stream let go, and the six callables let go; the three channels'
/// callables stay, as php's do.
fn reset_handlers(req: &mut Request, object: PhpObject) {
    let Some(curl) = object.native_mut::<Curl>(req) else {
        return;
    };
    curl.write.to = WriteTo::Stdout;
    curl.header.to = HeaderTo::Ignore;
    curl.read.to = ReadFrom::Direct;
    let released: Vec<Owner<Value>> = [
        curl.write.stream.take(),
        curl.header.stream.take(),
        curl.read.stream.take(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let stderr = curl.stderr.take();
    let easy = curl.easy.as_ref().map(Easy::id);
    let hooks: Vec<Handler> = curl.hooks.iter_mut().filter_map(Option::take).collect();
    for value in released {
        value.give_up(req);
    }
    match (stderr, easy) {
        (Some(value), Some(easy)) => easy.let_stream_go(req, value),
        (Some(value), None) => value.give_up(req),
        (None, _) => {}
    }
    for handler in hooks {
        handler.give_up(req);
    }
}

/// `curl_reset(CurlHandle $handle): void`.
///
/// ```php
/// $c = curl_init('file:///a');
/// curl_setopt($c, CURLOPT_PRIVATE, 'kept');
/// curl_reset($c);
/// var_dump(curl_getinfo($c, CURLINFO_EFFECTIVE_URL), curl_getinfo($c, CURLINFO_PRIVATE));
/// ```
/// ```expect
/// string(0) ""
/// string(4) "kept"
/// ```
///
/// From inside the `CurlHandle`'s own write callback `curl_reset` and
/// `curl_close` refuse, and the transfer, which holds its `CURL *`, runs
/// to its end (#2317).
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "body");
/// $c = curl_init("file://$f");
/// curl_setopt($c, CURLOPT_WRITEFUNCTION, function ($h, $data) {
///     try { curl_reset($h); } catch (Error $e) { echo $e->getMessage(), "\n"; }
///     try { @curl_close($h); } catch (Error $e) { echo $e->getMessage(), "\n"; }
///     echo $data, "\n";
///     return strlen($data);
/// });
/// var_dump(curl_exec($c), curl_errno($c));
/// unlink($f);
/// ```
/// ```expect
/// curl_reset(): Attempt to reset cURL handle from a callback
/// curl_close(): Attempt to close cURL handle from a callback
/// body
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn curl_reset(req: &mut Request, handle: Instance<CurlHandle>) -> Result<(), Thrown> {
    if curl(req, handle)?.in_callback {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "curl_reset(): Attempt to reset cURL handle from a callback",
        ));
    }
    let easy = easy_of(req, handle.object())?;
    let mut pending = Pending::default();
    if let Some(thrown) = easy.reset(req) {
        pending.park(req, thrown);
    }
    pending.step(req, |req, _| reset_handlers(req, handle.object()));
    if let Some(thrown) = pending
        .step(req, |req, _| set_defaults(req, easy))
        .flatten()
    {
        pending.park(req, thrown);
    }
    pending.answer(req, Ok(()))
}

/// `curl_exec(CurlHandle $handle): string|bool`: the transfer, its body
/// returned under `CURLOPT_RETURNTRANSFER`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "body\n");
/// $c = curl_init("file://$f");
/// var_dump(curl_exec($c));
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// var_dump(curl_exec($c));
/// unlink($f);
/// var_dump(curl_exec($c));
/// ```
/// ```expect
/// body
/// bool(true)
/// string(5) "body
/// "
/// bool(false)
/// ```
///
/// The body is kept on the request's heap and grown fallibly since
/// 8.5.12: past the memory limit the growth is refused, the transfer
/// ends in a write error, and the limit's fatal is raised as the call
/// returns. The figure is the growth of a huge block, which the address
/// space lets grow in place or not, so it varies between runs.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// $h = fopen($f, 'w');
/// for ($i = 0; $i < 10; $i++) fwrite($h, str_repeat('a', 1 << 20));
/// fclose($h);
/// register_shutdown_function(fn() => unlink($f));
/// $c = curl_init("file://$f");
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// var_dump(curl_exec($c), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate %d bytes) in %s on line 10
/// ```
#[php_function]
fn curl_exec(
    req: &mut Request,
    handle: Instance<CurlHandle>,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let object = handle.object();
    if let Some(thrown) = verify_handlers(req, object, true) {
        return Err(thrown);
    }
    cleanup_handle(req, object);
    let easy = easy_of(req, object)?;
    let (code, thrown) = easy.perform(req);
    curl(req, handle)?.err = i64::from(code);
    if let Some(thrown) = thrown {
        if code != 0 {
            free_body(req, object);
        }
        return Err(thrown);
    }
    if code != 0 {
        free_body(req, object);
        return Ok(Union::B(false));
    }
    easy.flush_stderr(req);
    finished(req, object)
}

/// What `curl_exec` answers once the transfer succeeded: the body kept
/// under `PHP_CURL_RETURN`, the file streams flushed otherwise.
fn finished(req: &mut Request, object: PhpObject) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let Some(curl) = object.native_mut::<Curl>(req) else {
        return Ok(Union::B(true));
    };
    if curl.write.to == WriteTo::Return {
        return hand_out(req, object).map(Union::A);
    }
    let streams: Vec<Value> = [
        (curl.write.to == WriteTo::File)
            .then(|| curl.write.stream())
            .flatten(),
        (curl.header.to == HeaderTo::File)
            .then(|| curl.header.stream())
            .flatten(),
    ]
    .into_iter()
    .flatten()
    .collect();
    for stream in streams {
        if let Some(stream) = Stream::from_value(req, &stream) {
            stream.flush(req);
        }
    }
    Ok(Union::B(true))
}

/// `curl_strerror(int $error_code): ?string`.
///
/// ```php
/// var_dump(curl_strerror(CURLE_OK), curl_strerror(CURLE_UNSUPPORTED_PROTOCOL), curl_strerror(-1));
/// ```
/// ```expect
/// string(8) "No error"
/// string(20) "Unsupported protocol"
/// string(13) "Unknown error"
/// ```
#[php_function]
fn curl_strerror(req: &mut Request, error_code: i64) -> Result<Option<Owner<PhpString>>, Thrown> {
    match crate::ffi::strerror(error_code) {
        Some(text) => PhpString::new(req, &text).map(Some),
        None => Ok(None),
    }
}

/// `curl_escape(CurlHandle $handle, string $string): string|false`.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_escape($c, "a b&c/ü"));
/// ```
/// ```expect
/// string(18) "a%20b%26c%2F%C3%BC"
/// ```
#[php_function]
fn curl_escape(
    req: &mut Request,
    handle: Instance<CurlHandle>,
    string: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let easy = easy_of(req, handle.object())?;
    let text = string.as_bytes().to_vec();
    match easy.escape(req, &text) {
        Some(escaped) => PhpString::new(req, &escaped).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `curl_unescape(CurlHandle $handle, string $string): string|false`.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_unescape($c, "a%20b%26c%2F%C3%BC%00z"));
/// ```
/// ```expect
/// string(10) "a b&c/ü z"
/// ```
#[php_function]
fn curl_unescape(
    req: &mut Request,
    handle: Instance<CurlHandle>,
    string: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let easy = easy_of(req, handle.object())?;
    let text = string.as_bytes().to_vec();
    match easy.unescape(req, &text) {
        Some(plain) => PhpString::new(req, &plain).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `curl_pause(CurlHandle $handle, int $flags): int`.
///
/// ```php
/// var_dump(curl_pause(curl_init(), CURLPAUSE_CONT));
/// ```
/// ```expect
/// int(43)
/// ```
#[php_function]
fn curl_pause(req: &mut Request, handle: Instance<CurlHandle>, flags: i64) -> Result<i64, Thrown> {
    let easy = easy_of(req, handle.object())?;
    let (code, thrown) = easy.pause(req, flags);
    match thrown {
        Some(thrown) => Err(thrown),
        None => Ok(i64::from(code)),
    }
}

/// `curl_upkeep(CurlHandle $handle): bool`.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_upkeep($c), curl_errno($c));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn curl_upkeep(req: &mut Request, handle: Instance<CurlHandle>) -> Result<bool, Thrown> {
    let easy = easy_of(req, handle.object())?;
    let (code, thrown) = easy.upkeep(req);
    curl(req, handle)?.err = i64::from(code);
    match thrown {
        Some(thrown) => Err(thrown),
        None => Ok(code == 0),
    }
}

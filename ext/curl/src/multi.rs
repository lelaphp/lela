//! `multi.c`: the `CurlMultiHandle` class over a `CURLM`, the easy
//! `CurlHandle`s added to it, its push callable, and its natives.

use php_ext::abi::{GcTable, Instance, ObjectHandlers, Ref, Refusals, Untyped};
use php_ext::attr::{php_class, php_function};
use php_ext::coerce::{callable_checked, to_long};
use php_ext::raising::{Level, Thrown, argument_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, TypeTag, Value};

use crate::State;
use crate::constants as c;
use crate::ffi::{Address, Cx, Easy, EasyId, Multi, MultiId, multi_strerror};
use crate::handle::{
    Curl, CurlHandle, Handler, WriteTo, cast_object, cleanup_handle, copy_handlers, curl, easy_of,
    hand_out, made, verify_handlers,
};
use crate::info::Assoc;
use crate::throwables::{Pending, Throwables};

/// `php_curlm` less its `zend_object`.
#[derive(Default)]
pub(crate) struct Mh {
    multi: Option<Multi>,
    /// `easyh`: each `CurlHandle` added, one owner of it.
    easy: Vec<Owner<PhpObject>>,
    /// `handlers.server_push`.
    push: Option<Handler>,
    /// `err.no`.
    err: i64,
    /// Whether `curl_multi_exec` is running a transfer of this multi, so
    /// that a callback of one of its easy handles may be on the stack.
    running: bool,
}

/// `final class CurlMultiHandle`, `@strict-properties`,
/// `@not-serializable`.
///
/// ```php
/// try { new CurlMultiHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $m = curl_multi_init();
/// try { clone $m; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($m, (int) $m, $m == curl_multi_init());
/// ```
/// ```expect
/// Cannot directly construct CurlMultiHandle, use curl_multi_init() instead
/// Trying to clone an uncloneable object of class CurlMultiHandle
/// object(CurlMultiHandle)#1 (0) {
/// }
/// int(1)
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    payload = Mh,
    handlers = HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct CurlMultiHandle;

#[php_class]
impl CurlMultiHandle {}

/// `curl_multi_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What `curl_multi_handlers` refuses: construction, cloning and
/// comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("curl_multi_init()"))
    .cloning()
    .comparison();

impl ObjectHandlers for Handlers {
    /// `curl_multi_free_obj`: `curl_multi_cleanup`, then the `CurlHandle`s and
    /// the push callable let go.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(mh) = object.native_mut::<Mh>(req).map(std::mem::take) else {
            return;
        };
        if let Some(multi) = mh.multi {
            let _ = multi.close(req);
        }
        for easy in mh.easy {
            easy.give_up(req);
        }
        if let Some(push) = mh.push {
            push.give_up(req);
        }
    }

    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(mh) = object.native::<Mh>(req) else {
            return GcTable::Properties;
        };
        if let Some(push) = &mh.push {
            push.visit(visit);
        }
        for easy in &mh.easy {
            visit(easy.as_value());
        }
        GcTable::Properties
    }

    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        cast_object(req, object, to)
    }
}

fn mh(req: &mut Request, multi: Instance<CurlMultiHandle>) -> Result<&mut Mh, Thrown> {
    multi.payload_mut(req)
}

/// The `CurlHandle`s added, as views of the owners `easyh` holds.
fn handles(mh: &Mh) -> Vec<PhpObject> {
    mh.easy.iter().map(|handle| **handle).collect()
}

/// The library's `CURLM *` of an argument, by name.
fn multi_of(req: &mut Request, multi: Instance<CurlMultiHandle>) -> Result<MultiId, Thrown> {
    match &mh(req, multi)?.multi {
        Some(handle) => Ok(handle.id()),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a CurlMultiHandle with no multi handle",
        )),
    }
}

/// `curl_multi_init(): CurlMultiHandle`.
///
/// ```php
/// var_dump(curl_multi_init() instanceof CurlMultiHandle);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn curl_multi_init(req: &mut Request) -> Result<Owner<Instance<CurlMultiHandle>>, Thrown> {
    let object = made(req, b"CurlMultiHandle")?;
    let session = req.state::<State>().session.clone();
    let Some(multi) = Multi::init(req, session, *object) else {
        object.give_up(req);
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "curl_multi_init(): Could not initialize a new cURL multi handle",
        ));
    };
    match object.native_mut::<Mh>(req) {
        Some(mh) => mh.multi = Some(multi),
        None => drop(multi.close(req)),
    }
    Ok(object.into_instance())
}

/// `curl_multi_add_handle(CurlMultiHandle $multi_handle, CurlHandle $handle): int`.
///
/// ```php
/// $m = curl_multi_init();
/// $c = curl_init();
/// var_dump(curl_multi_add_handle($m, $c), curl_multi_add_handle($m, $c), curl_multi_errno($m));
/// ```
/// ```expect
/// int(0)
/// int(7)
/// int(7)
/// ```
#[php_function]
fn curl_multi_add_handle(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    handle: Instance<CurlHandle>,
) -> Result<i64, Thrown> {
    let object = handle.object();
    if let Some(thrown) = verify_handlers(req, object, true) {
        return Err(thrown);
    }
    cleanup_handle(req, object);
    let multi = multi_of(req, multi_handle)?;
    let easy = easy_of(req, object)?;
    let code = multi.add(req, easy);
    mh(req, multi_handle)?.err = code;
    if code == 0 {
        let held = Owner::take(req, object);
        match mh(req, multi_handle) {
            Ok(mh) => mh.easy.push(held),
            Err(thrown) => {
                held.give_up(req);
                return Err(thrown);
            }
        }
    }
    Ok(code)
}

/// `curl_multi_remove_handle(CurlMultiHandle $multi_handle, CurlHandle $handle): int`.
///
/// ```php
/// $m = curl_multi_init();
/// $c = curl_init();
/// curl_multi_add_handle($m, $c);
/// var_dump(curl_multi_remove_handle($m, $c), count(curl_multi_get_handles($m)));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// ```
#[php_function]
fn curl_multi_remove_handle(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    handle: Instance<CurlHandle>,
) -> Result<i64, Thrown> {
    let object = handle.object();
    let multi = multi_of(req, multi_handle)?;
    let easy = easy_of(req, object)?;
    let code = multi.remove(req, easy);
    let mh = mh(req, multi_handle)?;
    mh.err = code;
    if code == 0
        && let Some(at) = mh
            .easy
            .iter()
            .position(|held| held.index() == object.index())
    {
        let held = mh.easy.remove(at);
        held.give_up(req);
    }
    Ok(code)
}

/// `curl_multi_get_handles(CurlMultiHandle $multi_handle): array`.
///
/// ```php
/// $m = curl_multi_init();
/// $a = curl_init(); $b = curl_init();
/// curl_multi_add_handle($m, $a); curl_multi_add_handle($m, $b);
/// var_dump(curl_multi_get_handles($m) === [$a, $b]);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn curl_multi_get_handles(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
) -> Result<Owner<PhpArray>, Thrown> {
    let handles = handles(mh(req, multi_handle)?);
    let mut array = PhpArray::new(req)?;
    for handle in handles {
        array.append(req, Owner::take(req, handle.as_value()))?;
    }
    Ok(array)
}

/// `curl_multi_select(CurlMultiHandle $multi_handle, float $timeout = 1.0): int`.
///
/// ```php
/// $m = curl_multi_init();
/// var_dump(curl_multi_select($m, 0.01));
/// try { curl_multi_select($m, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// curl_multi_select(): Argument #2 ($timeout) must be between 0 and 2147483.647000
/// ```
#[php_function]
fn curl_multi_select(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    #[php(default = 1.0)] timeout: f64,
) -> Result<i64, Thrown> {
    let most = f64::from(i32::MAX) / 1000.0;
    if !(0.0..=most).contains(&timeout) {
        let ce = Throwables::of(req)?;
        return Err(argument_error(
            req,
            ce.value_error,
            2,
            &format!("must be between 0 and {most:.6}"),
        ));
    }
    let multi = multi_of(req, multi_handle)?;
    #[allow(clippy::cast_possible_truncation)]
    let (code, numfds) = multi.wait(req, (timeout * 1000.0) as i32);
    if code != 0 {
        mh(req, multi_handle)?.err = code;
        return Ok(-1);
    }
    Ok(numfds)
}

/// `curl_multi_exec(CurlMultiHandle $multi_handle, &$still_running): int`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "multi\n");
/// $m = curl_multi_init();
/// $c = curl_init("file://$f");
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// curl_multi_add_handle($m, $c);
/// do { $status = curl_multi_exec($m, $active); } while ($active && $status == CURLM_OK);
/// var_dump($status, $active, curl_multi_getcontent($c));
/// unlink($f);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// string(6) "multi
/// "
/// ```
///
/// A body kept under `CURLOPT_RETURNTRANSFER` is grown fallibly since
/// 8.5.12: past the memory limit the transfer ends in a write error, and
/// the limit's fatal is raised as the call returns. The figure is the
/// growth of a huge block, which the address space lets grow in place or
/// not, so it varies between runs.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// $h = fopen($f, 'w');
/// for ($i = 0; $i < 10; $i++) fwrite($h, str_repeat('a', 1 << 20));
/// fclose($h);
/// register_shutdown_function(fn() => unlink($f));
/// $m = curl_multi_init();
/// $c = curl_init("file://$f");
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// curl_multi_add_handle($m, $c);
/// do { curl_multi_exec($m, $running); } while ($running);
/// echo "unreached\n";
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate %d bytes) in %s on line 12
/// ```
#[php_function]
fn curl_multi_exec(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    still_running: Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    let handles = handles(mh(req, multi_handle)?);
    let mut held: Vec<EasyId> = Vec::with_capacity(handles.len());
    for handle in &handles {
        if let Some(thrown) = verify_handlers(req, *handle, true) {
            return Err(thrown);
        }
        if let Ok(easy) = easy_of(req, *handle) {
            held.push(easy);
        }
    }
    let running = match still_running.get() {
        Some(Untyped(value)) => to_long(req, &value.deref())?,
        None => 0,
    };
    let multi = multi_of(req, multi_handle)?;
    let was = std::mem::replace(&mut mh(req, multi_handle)?.running, true);
    #[allow(clippy::cast_possible_truncation)]
    let ((code, running), thrown) = multi.perform(req, &held, running as i32);
    mh(req, multi_handle)?.running = was;
    if let Some(thrown) = thrown {
        return Err(thrown);
    }
    still_running.set(req, Untyped(Value::int(running)))?;
    mh(req, multi_handle)?.err = code;
    Ok(code)
}

/// `curl_multi_getcontent(CurlHandle $handle): ?string`: what
/// `CURLOPT_RETURNTRANSFER` kept, `null` without it.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_multi_getcontent($c));
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// var_dump(curl_multi_getcontent($c));
/// ```
/// ```expect
/// NULL
/// string(0) ""
/// ```
#[php_function]
fn curl_multi_getcontent(
    req: &mut Request,
    handle: Instance<CurlHandle>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    if curl(req, handle)?.write.to != WriteTo::Return {
        return Ok(None);
    }
    hand_out(req, handle.object()).map(Some)
}

/// `curl_multi_info_read(CurlMultiHandle $multi_handle, &$queued_messages = null): array|false`.
///
/// ```php
/// $m = curl_multi_init();
/// $c = curl_init('file:///nonexistent-curl-witness');
/// curl_setopt($c, CURLOPT_RETURNTRANSFER, true);
/// curl_multi_add_handle($m, $c);
/// do { curl_multi_exec($m, $active); } while ($active);
/// $info = curl_multi_info_read($m, $queued);
/// var_dump($info['msg'], $info['result'], $info['handle'] === $c, $queued, curl_errno($c), curl_multi_info_read($m));
/// ```
/// ```expect
/// int(1)
/// int(37)
/// bool(true)
/// int(0)
/// int(37)
/// bool(false)
/// ```
#[php_function]
fn curl_multi_info_read(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    #[php(default = null)] queued_messages: Ref<Untyped<Value>>,
) -> Result<Union2, Thrown> {
    let multi = multi_of(req, multi_handle)?;
    let Some(done) = multi.info_read(req) else {
        return Ok(Union2::False);
    };
    if queued_messages.is_given() {
        queued_messages.set(req, Untyped(Value::int(done.queued)))?;
    }
    let mut out = Assoc::new(req)?;
    out.add(req, "msg", Owner::int(done.msg))?;
    out.add(req, "result", Owner::int(done.result))?;
    if let Some(handle) = find(req, multi_handle.object(), done.easy) {
        if let Some(curl) = handle.native_mut::<Curl>(req) {
            curl.err = done.result;
        }
        out.add(req, "handle", Owner::take(req, handle.as_value()))?;
    }
    Ok(Union2::Value(out.0))
}

/// `array|false`.
type Union2 = php_ext::abi::OrFalse<Owner<PhpArray>>;

/// `_php_curl_multi_find_easy_handle`: the `CurlHandle` added whose
/// `CURL *` is the one the library named.
fn find(req: &mut Request, multi: PhpObject, easy: Address) -> Option<PhpObject> {
    let handles = handles(multi.native::<Mh>(req)?);
    handles.into_iter().find(|handle| {
        let held = handle
            .native::<Curl>(req)
            .and_then(|curl| curl.easy.as_ref().map(Easy::id));
        held.and_then(|held| held.address(req)) == Some(easy)
    })
}

/// The `CURL *` of a `CurlHandle` found, by name.
fn easy_id(req: &Request, handle: PhpObject) -> Option<EasyId> {
    handle
        .native::<Curl>(req)
        .and_then(|curl| curl.easy.as_ref().map(Easy::id))
}

/// `curl_multi_close(CurlMultiHandle $multi_handle): void`: every `CurlHandle`
/// removed and let go.
///
/// ```php
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, curl_init());
/// curl_multi_close($m);
/// var_dump(curl_multi_get_handles($m));
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
///
/// From inside a callback of a transfer this multi is running it is
/// refused with the Error `curl_close()` throws from a callback, and the
/// transfer goes on (`trace:curl-multi-closed-in-callback`; ext/curl's
/// leak sensor test
/// `a_multi_closed_by_its_own_write_callback_is_refused_and_the_transfer_runs_to_its_end`).
/// Another multi closes from a callback as from anywhere else.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, str_repeat('x', 40000));
/// $m = curl_multi_init();
/// $other = curl_multi_init();
/// curl_multi_add_handle($other, curl_init());
/// $d = curl_init("file://$f");
/// curl_setopt($d, CURLOPT_WRITEFUNCTION, function ($h, $data) use ($other) {
///     static $n = 0;
///     if ($n++ === 0) { curl_multi_close($other); echo "closed\n"; }
///     return strlen($data);
/// });
/// curl_multi_add_handle($m, $d);
/// do { $status = curl_multi_exec($m, $active); } while ($active && $status == CURLM_OK);
/// var_dump($status, count(curl_multi_get_handles($m)), curl_multi_get_handles($other));
/// unlink($f);
/// ```
/// ```expect
/// closed
/// int(0)
/// int(1)
/// array(0) {
/// }
/// ```
#[php_function]
fn curl_multi_close(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
) -> Result<(), Thrown> {
    if mh(req, multi_handle)?.running {
        let text = format!(
            "{}(): Attempt to close cURL handle from a callback",
            req.frames().active_function_name()
        );
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, &text));
    }
    let multi = multi_of(req, multi_handle)?;
    let handles = std::mem::take(&mut mh(req, multi_handle)?.easy);
    let mut pending = Pending::default();
    for handle in handles {
        if let Some(thrown) = verify_handlers(req, *handle, true) {
            pending.park(req, thrown);
        }
        if let Ok(easy) = easy_of(req, *handle) {
            multi.remove(req, easy);
        }
        handle.give_up(req);
    }
    pending.answer(req, Ok(()))
}

/// `curl_multi_errno(CurlMultiHandle $multi_handle): int`.
///
/// ```php
/// var_dump(curl_multi_errno(curl_multi_init()));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function]
fn curl_multi_errno(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
) -> Result<i64, Thrown> {
    Ok(mh(req, multi_handle)?.err)
}

/// `curl_multi_strerror(int $error_code): ?string`.
///
/// ```php
/// var_dump(curl_multi_strerror(CURLM_OK), curl_multi_strerror(CURLM_BAD_HANDLE), curl_multi_strerror(-5));
/// ```
/// ```expect
/// string(8) "No error"
/// string(20) "Invalid multi handle"
/// string(13) "Unknown error"
/// ```
#[php_function]
fn curl_multi_strerror(
    req: &mut Request,
    error_code: i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    match multi_strerror(error_code) {
        Some(text) => PhpString::new(req, &text).map(Some),
        None => Ok(None),
    }
}

/// `curl_multi_setopt(CurlMultiHandle $multi_handle, int $option, mixed $value): bool`.
///
/// ```php
/// $m = curl_multi_init();
/// var_dump(curl_multi_setopt($m, CURLMOPT_MAXCONNECTS, 4), curl_multi_setopt($m, CURLMOPT_PIPELINING, CURLPIPE_HTTP1));
/// try { curl_multi_setopt($m, 12345, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_multi_setopt($m, CURLMOPT_PUSHFUNCTION, 'nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(curl_multi_errno($m));
/// ```
/// ```expect
///
/// Warning: curl_multi_setopt(): CURLPIPE_HTTP1 is no longer supported in %s on line 3
/// bool(true)
/// bool(true)
/// curl_multi_setopt(): Argument #2 ($option) is not a valid cURL multi option
/// curl_multi_setopt(): Argument #2 ($option) must be a valid callback for option CURLMOPT_PUSHFUNCTION, function "nope" not found or invalid function name
/// int(6)
/// ```
///
/// `CURLMOPT_PUSHFUNCTION` keeps what php's `zend_fcc_dup` keeps: a
/// `'Class::method'` string set with `$this` in scope keeps the object
/// it resolved to until the `CurlMultiHandle` is freed.
///
/// ```php
/// class A {
///     function p($parent, $pushed, $headers) { return CURL_PUSH_DENY; }
///     function go() {
///         $mh = curl_multi_init();
///         curl_multi_setopt($mh, CURLMOPT_PUSHFUNCTION, 'A::p');
///         return $mh;
///     }
///     function __destruct() { echo "dtor\n"; }
/// }
/// $mh = (new A)->go();
/// echo "kept\n";
/// unset($mh);
/// echo "end\n";
/// ```
/// ```expect
/// kept
/// dtor
/// end
/// ```
#[php_function]
fn curl_multi_setopt(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    option: i64,
    value: Value,
) -> Result<bool, Thrown> {
    let multi = multi_of(req, multi_handle)?;
    let code = match option {
        c::CURLMOPT_PIPELINING
        | c::CURLMOPT_MAXCONNECTS
        | c::CURLMOPT_CHUNK_LENGTH_PENALTY_SIZE
        | c::CURLMOPT_CONTENT_LENGTH_PENALTY_SIZE
        | c::CURLMOPT_MAX_HOST_CONNECTIONS
        | c::CURLMOPT_MAX_PIPELINE_LENGTH
        | c::CURLMOPT_MAX_TOTAL_CONNECTIONS
        | c::CURLMOPT_MAX_CONCURRENT_STREAMS => {
            let n = to_long(req, &value.deref())?;
            if option == c::CURLMOPT_PIPELINING && n & 1 != 0 {
                docref(req, Level::Warning, "CURLPIPE_HTTP1 is no longer supported");
            }
            multi.set_long(req, option, n)
        }
        c::CURLMOPT_PUSHFUNCTION => {
            if let Some(old) = mh(req, multi_handle)?.push.take() {
                old.give_up(req);
            }
            let value = value.deref();
            let callable = match callable_checked(req, &value) {
                Ok(callable) => callable,
                Err(refusal) => {
                    php_ext::raising::pending(req)?;
                    let tail = format!(
                        "must be a valid callback for option CURLMOPT_PUSHFUNCTION, {refusal}"
                    );
                    let ce = Throwables::of(req)?;
                    return Err(argument_error(req, ce.type_error, 2, &tail));
                }
            };
            mh(req, multi_handle)?.push = Some(Owner::take(req, callable));
            let code = multi.set_push(req);
            if code != 0 {
                return Ok(false);
            }
            code
        }
        _ => {
            mh(req, multi_handle)?.err = 6;
            let ce = Throwables::of(req)?;
            return Err(argument_error(
                req,
                ce.value_error,
                2,
                "is not a valid cURL multi option",
            ));
        }
    };
    mh(req, multi_handle)?.err = code;
    Ok(code == 0)
}

/// `_php_server_push_callback`: a new `CurlHandle` over the pushed
/// transfer, its parent's channels copied, handed to the push callable
/// with the push's headers; kept by the `CURLM *` where the callable
/// accepts it. The library made the pushed `CURL *` a copy of its
/// parent's, which points at the parent's lists and file parts from
/// then on.
pub(crate) fn push(
    cx: &mut Cx<'_>,
    multi: PhpObject,
    parent: Address,
    made_easy: impl FnOnce(&mut Request, PhpObject) -> Option<Easy>,
    headers: &[Vec<u8>],
) -> bool {
    let Some(parent) = find(cx.req, multi, parent) else {
        return false;
    };
    if let Some(easy) = easy_id(cx.req, parent) {
        easy.copied(cx.req);
    }
    let Some(callable) = multi
        .native::<Mh>(cx.req)
        .and_then(|mh| mh.push.as_ref().map(|push| (**push).clone()))
    else {
        return false;
    };
    let Ok(pushed) = made(cx.req, b"CurlHandle") else {
        return false;
    };
    let Some(easy) = made_easy(cx.req, *pushed) else {
        pushed.give_up(cx.req);
        return false;
    };
    if let Some(from) = easy_id(cx.req, parent) {
        easy.id().stderr_from(cx.req, from);
    }
    match copy_handlers(cx.req, parent) {
        Ok(mut copied) => {
            if let Some(curl) = pushed.native_mut::<Curl>(cx.req) {
                copied.easy = Some(easy);
                *curl = copied;
            } else {
                let _ = easy.close(cx.req);
                copied.release_values(cx.req);
            }
        }
        Err(_) => {
            let _ = easy.close(cx.req);
        }
    }
    let Ok(list) = crate::info::list(cx.req, headers) else {
        pushed.give_up(cx.req);
        return false;
    };
    let returned = cx.call(&callable, &[parent.as_value(), pushed.as_value(), *list]);
    list.give_up(cx.req);
    let accepted = match returned {
        Some(value) => {
            let n = to_long(cx.req, &value).unwrap_or(c::CURL_PUSH_DENY);
            value.give_up(cx.req);
            n != c::CURL_PUSH_DENY
        }
        None => false,
    };
    if !accepted {
        pushed.give_up(cx.req);
        return false;
    }
    if let Some(easy) = easy_id(cx.req, *pushed) {
        easy.accept(cx.req);
    }
    match multi.native_mut::<Mh>(cx.req) {
        Some(mh) => mh.easy.push(pushed),
        None => pushed.give_up(cx.req),
    }
    true
}

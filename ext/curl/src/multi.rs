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
    hand_out, made, verify_before_cleanup, verify_handlers,
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
///
/// What a callable throws from inside `curl_multi_cleanup` is left
/// pending past `curl_multi_free_obj`, as php leaves it in
/// `EG(exception)`, and the statement that freed the multi raises it.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     echo "debug: $data";
///     throw new Exception('from the callable');
/// });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// try {
///     unset($m);
///     echo "after unset\n";
/// } catch (Exception $e) {
///     echo 'thrown: ', $e->getMessage(), "\n";
/// }
/// echo "end\n";
/// ```
/// ```expect
/// debug: closing connection #0
/// thrown: from the callable
/// end
/// ```
///
/// Each `CurlHandle` in the list is verified silently before
/// `curl_multi_cleanup`, so a `CURLOPT_STDERR` stream closed meanwhile
/// is let go and the file reset to the process's `stderr`, which the
/// cleanup's line then reaches. libcurl writes it through the C
/// library's `stderr`, the process's and not the request's, so the fence
/// is served in a process of its own.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// $fp = fopen('php://memory', 'w+');
/// curl_setopt($h, CURLOPT_STDERR, $fp);
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// fclose($fp);
/// unset($m);
/// echo "end\n";
/// ```
/// ```expect process
/// end
/// ```
/// ```stderr
/// * closing connection #0
/// ```
///
/// The `CURLM *` stays the payload's over `curl_multi_cleanup`, as php
/// leaves `mh->multi` in place, so a callable the cleanup calls back
/// reaches it: here a collected cycle, the multi listing the
/// `CurlHandle` and its callable holding the multi. libcurl answers a
/// call on a multi from inside its callback `CURLM_RECURSIVE_API_CALL`,
/// and `curl_multi_close` lets the list go as it would outside the
/// cleanup.
///
/// ```php
/// $m = curl_multi_init();
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use ($m, $h) {
///     if (!str_starts_with($data, 'closing')) {
///         return;
///     }
///     echo "debug: $data";
///     var_dump(curl_multi_remove_handle($m, $h), curl_multi_errno($m));
///     curl_multi_close($m);
///     var_dump(count(curl_multi_get_handles($m)));
/// });
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// unset($m, $h);
/// var_dump(gc_collect_cycles());
/// echo "end\n";
/// ```
/// ```expect
/// debug: closing connection #0
/// int(8)
/// int(8)
/// int(0)
/// int(3)
/// end
/// ```
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What `curl_multi_handlers` refuses: construction, cloning and
/// comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("curl_multi_init()"))
    .cloning()
    .comparison();

impl ObjectHandlers for Handlers {
    /// `curl_multi_free_obj`: each `CurlHandle` in `easyh` verified
    /// silently ([`verify_before_cleanup`]), `curl_multi_cleanup` with
    /// `easyh` whole and the `CURLM *` still the payload's, as php leaves
    /// `mh->multi` in place ([`MultiId::cleanup`]), then the `CURLM *`,
    /// the `CurlHandle`s and the push callable let go under what its
    /// callables threw, which is left pending for the release that freed
    /// the object to raise, as php leaves it in `EG(exception)`.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let mut pending = Pending::default();
        let multi = object
            .native::<Mh>(req)
            .and_then(|mh| mh.multi.as_ref().map(Multi::id));
        let mut closed = (None, None);
        if let Some(multi) = multi {
            let easy = object.native::<Mh>(req).map(handles).unwrap_or_default();
            for &handle in &easy {
                verify_before_cleanup(req, handle, &mut pending);
            }
            // A `CurlHandle` whose free is under way has no `CURL *` left
            // to hold, php's `IS_OBJ_FREE_CALLED`.
            let easy: Vec<EasyId> = easy
                .into_iter()
                .filter_map(|handle| object_easy(req, handle))
                .collect();
            pending.release(req, |req| {
                closed.0 = multi.cleanup(req, &easy);
                let multi = object.native_mut::<Mh>(req).and_then(|mh| mh.multi.take());
                closed.1 = multi.and_then(|multi| multi.close(req));
            });
        }
        for thrown in [closed.0, closed.1].into_iter().flatten() {
            pending.park(req, thrown);
        }
        if let Some(mh) = object.native_mut::<Mh>(req).map(std::mem::take) {
            let (mut easy, mut push) = (mh.easy, mh.push);
            pending.release(req, |req| {
                for held in easy.drain(..) {
                    held.give_up(req);
                }
                if let Some(push) = push.take() {
                    push.give_up(req);
                }
            });
        }
        pending.leave(req);
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

/// The `CURL *` of a `CurlHandle` by name, `None` where it has none.
fn object_easy(req: &mut Request, handle: PhpObject) -> Option<EasyId> {
    let curl = handle.native::<Curl>(req)?;
    curl.easy.as_ref().map(Easy::id)
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
        None => {
            if let Some(thrown) = multi.close(req) {
                object.give_up(req);
                return Err(thrown);
            }
        }
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
///
/// What `_php_curl_verify_handlers` raised stays pending while the
/// `CurlHandle` is added, as php goes on to `curl_multi_add_handle` with
/// it in `EG(exception)`.
///
/// ```php
/// set_error_handler(function ($no, $text) { throw new Exception($text); });
/// $h = curl_init();
/// $fp = fopen('php://memory', 'w');
/// curl_setopt($h, CURLOPT_FILE, $fp);
/// fclose($fp);
/// $m = curl_multi_init();
/// try {
///     curl_multi_add_handle($m, $h);
/// } catch (Exception $e) {
///     echo $e->getMessage(), "\n";
/// }
/// var_dump(curl_multi_get_handles($m) === [$h], curl_multi_errno($m));
/// ```
/// ```expect
/// curl_multi_add_handle(): CURLOPT_FILE resource has gone away, resetting to default
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn curl_multi_add_handle(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    handle: Instance<CurlHandle>,
) -> Result<i64, Thrown> {
    let object = handle.object();
    let mut pending = Pending::default();
    if let Some(thrown) = verify_handlers(req, object, true) {
        pending.park(req, thrown);
    }
    let added = pending.step(req, |req, pending| {
        cleanup_handle(req, object);
        let multi = multi_of(req, multi_handle)?;
        let easy = easy_of(req, object)?;
        let (code, thrown) = multi.add(req, easy);
        if let Some(thrown) = thrown {
            pending.park(req, thrown);
        }
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
    });
    // `None` only where a fatal stands, which is the answer then.
    pending.answer(req, added.unwrap_or(Ok(0)))
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
///
/// libcurl logs from inside the remove: taking out a transfer that has a
/// connection writes `closing connection` to its debug callable, which
/// the request is lent to for the call.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) { echo "debug: $data"; });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// echo "exec\n";
/// curl_multi_remove_handle($m, $h);
/// echo "removed\n";
/// ```
/// ```expect
/// exec
/// debug: closing connection #0
/// removed
/// ```
///
/// What the callable throws there is this native's throwable, and the
/// `CurlHandle` is taken out all the same, as php leaves it pending past
/// `zend_llist_del_element`.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     echo "debug: $data";
///     throw new Exception('from the callable');
/// });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// try {
///     curl_multi_remove_handle($m, $h);
/// } catch (Exception $e) {
///     echo get_class($e), ': ', $e->getMessage(), "\n";
/// }
/// var_dump(count(curl_multi_get_handles($m)), curl_multi_errno($m));
/// ```
/// ```expect
/// debug: closing connection #0
/// Exception: from the callable
/// int(0)
/// int(0)
/// ```
///
/// From inside another transfer's write callback, the line reaches the
/// callable through the request that callback was lent.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) { echo "debug: $data"; });
/// $other = curl_multi_init();
/// curl_multi_add_handle($other, $h);
/// curl_multi_exec($other, $r);
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, 'x');
/// $m = curl_multi_init();
/// $d = curl_init("file://$f");
/// curl_setopt($d, CURLOPT_WRITEFUNCTION, function ($d, $data) use ($other, $h) {
///     var_dump(curl_multi_remove_handle($other, $h));
///     return strlen($data);
/// });
/// curl_multi_add_handle($m, $d);
/// do { $status = curl_multi_exec($m, $active); } while ($active && $status == CURLM_OK);
/// var_dump($status, count(curl_multi_get_handles($other)));
/// unlink($f);
/// ```
/// ```expect
/// debug: closing connection #0
/// int(0)
/// int(0)
/// int(0)
/// ```
///
/// An `exit` the callable takes inside the remove stands as php's unwind
/// exit does, and the `CurlHandle` is taken off the list past it.
///
/// ```php
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     if (str_starts_with($data, 'closing')) { echo "debug: $data"; exit("exit\n"); }
/// });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// register_shutdown_function(function () use ($m) { var_dump(count(curl_multi_get_handles($m))); });
/// curl_multi_remove_handle($m, $h);
/// echo "unreached\n";
/// ```
/// ```expect
/// debug: closing connection #0
/// exit
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
    let (code, thrown) = multi.remove(req, easy);
    let mut pending = Pending::default();
    if let Some(thrown) = thrown {
        pending.park(req, thrown);
    }
    // `SAVE_CURLM_ERROR` and `zend_llist_del_element`, with what the
    // callable threw standing, as php goes on past it.
    let taken = pending.step(req, |req, _| {
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
    });
    // `None` only where a fatal stands, which is the answer then.
    pending.answer(req, taken.unwrap_or(Ok(code)))
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
///
/// What a callable threw during the transfer is this native's throwable,
/// and the count of transfers still going is written all the same, as
/// php's `ZEND_TRY_ASSIGN_REF_LONG` follows `curl_multi_perform` with it
/// pending.
///
/// ```php
/// $a = curl_init('http://127.0.0.1:1/');
/// $b = curl_init('http://127.0.0.1:1/');
/// curl_setopt($b, CURLOPT_VERBOSE, 1);
/// curl_setopt($b, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     static $n = 0;
///     if ($n++ === 0) {
///         throw new Exception('from the callable');
///     }
/// });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $a);
/// curl_multi_add_handle($m, $b);
/// $running = 'not assigned';
/// try {
///     curl_multi_exec($m, $running);
/// } catch (Exception $e) {
///     echo $e->getMessage(), "\n";
/// }
/// var_dump($running);
/// ```
/// ```expect
/// from the callable
/// int(2)
/// ```
///
/// The code is saved even where writing the count raised, as php's
/// `SAVE_CURLM_ERROR` follows `ZEND_TRY_ASSIGN_REF_LONG` whatever it
/// threw: a call from inside the multi's own transfer answers
/// `CURLM_RECURSIVE_API_CALL`, and a typed reference refuses the count.
///
/// ```php
/// declare(strict_types=1);
/// class C { public string $r = ''; }
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, 'x');
/// $m = curl_multi_init();
/// $d = curl_init("file://$f");
/// curl_setopt($d, CURLOPT_WRITEFUNCTION, function ($d, $data) use ($m) {
///     $c = new C;
///     try {
///         curl_multi_exec($m, $c->r);
///     } catch (TypeError $e) {
///         echo $e->getMessage(), "\n";
///     }
///     var_dump(curl_multi_errno($m));
///     return strlen($data);
/// });
/// curl_multi_add_handle($m, $d);
/// do { $status = curl_multi_exec($m, $active); } while ($active && $status == CURLM_OK);
/// unlink($f);
/// ```
/// ```expect
/// Cannot assign int to reference held by property C::$r of type string
/// int(8)
/// ```
///
/// An `exit` a callable takes inside the transfer stands as php's unwind
/// exit does, and the count is written past it.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, 'x');
/// $h = curl_init("file://$f");
/// curl_setopt($h, CURLOPT_WRITEFUNCTION, function ($h, $data) { echo "write: $data\n"; exit("exit\n"); });
/// $m = curl_multi_init();
/// curl_multi_add_handle($m, $h);
/// $running = 'not assigned';
/// register_shutdown_function(function () use ($m, $h, $f, &$running) {
///     var_dump($running);
///     curl_multi_remove_handle($m, $h);
///     unlink($f);
/// });
/// curl_multi_exec($m, $running);
/// echo "unreached\n";
/// ```
/// ```expect
/// write: x
/// exit
/// int(0)
/// ```
#[php_function]
fn curl_multi_exec(
    req: &mut Request,
    multi_handle: Instance<CurlMultiHandle>,
    still_running: Ref<Untyped<Value>>,
) -> Result<i64, Thrown> {
    let handles = handles(mh(req, multi_handle)?);
    let mut pending = Pending::default();
    let mut held: Vec<EasyId> = Vec::with_capacity(handles.len());
    for handle in &handles {
        pending.step(req, |req, pending| {
            if let Some(thrown) = verify_handlers(req, *handle, true) {
                pending.park(req, thrown);
            }
        });
        // A `CurlHandle` whose free is under way has no `CURL *` left
        // to hold, as in `curl_multi_free_obj`.
        if let Some(easy) = object_easy(req, *handle) {
            held.push(easy);
        }
    }
    let performed = pending.step(req, |req, pending| {
        let running = match still_running.get() {
            Some(Untyped(value)) => to_long(req, &value.deref())?,
            None => 0,
        };
        let multi = multi_of(req, multi_handle)?;
        let was = std::mem::replace(&mut mh(req, multi_handle)?.running, true);
        #[allow(clippy::cast_possible_truncation)]
        let (performed, thrown) = multi.perform(req, &held, running as i32);
        mh(req, multi_handle)?.running = was;
        if let Some(thrown) = thrown {
            pending.park(req, thrown);
        }
        Ok(performed)
    });
    // The count and `SAVE_CURLM_ERROR` as a step of their own, with what
    // the transfer threw standing; `None` only where a fatal stands,
    // which is the answer then.
    let answered = match performed {
        Some(Ok((code, running))) => pending
            .step(req, |req, _| {
                // `SAVE_CURLM_ERROR` whatever the assignment raised.
                let assigned = still_running.set(req, Untyped(Value::int(running)));
                mh(req, multi_handle)?.err = code;
                assigned.map(|()| code)
            })
            .unwrap_or(Ok(code)),
        Some(Err(thrown)) => Err(thrown),
        None => Ok(0),
    };
    pending.answer(req, answered)
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
///
/// Each remove lends the request to the callables, and what the first
/// throws stands over the rest, whose callables are then not called, as
/// php's `zend_call_function` returns at once with `EG(exception)` set.
///
/// ```php
/// $m = curl_multi_init();
/// foreach (['a', 'b'] as $name) {
///     $h = curl_init('http://127.0.0.1:1/');
///     curl_setopt($h, CURLOPT_VERBOSE, 1);
///     curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use ($name) {
///         if (str_starts_with($data, 'closing')) {
///             echo "$name: $data";
///             throw new Exception($name);
///         }
///     });
///     curl_multi_add_handle($m, $h);
/// }
/// curl_multi_exec($m, $r);
/// try {
///     curl_multi_close($m);
/// } catch (Exception $e) {
///     echo 'thrown: ', $e->getMessage(), "\n";
/// }
/// var_dump(curl_multi_get_handles($m));
/// ```
/// ```expect
/// a: closing connection #0
/// thrown: a
/// array(0) {
/// }
/// ```
///
/// The list is walked in place and emptied after every remove, as
/// `zend_llist_clean` empties it, so a callable inside a remove reads it
/// whole.
///
/// ```php
/// $m = curl_multi_init();
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use ($m) {
///     if (str_starts_with($data, 'closing')) { echo "debug: ", count(curl_multi_get_handles($m)), "\n"; }
/// });
/// curl_multi_add_handle($m, $h);
/// curl_multi_exec($m, $r);
/// curl_multi_close($m);
/// var_dump(count(curl_multi_get_handles($m)));
/// ```
/// ```expect
/// debug: 1
/// int(0)
/// ```
///
/// A callable inside a remove that adds to the multi or removes from
/// it is answered `CURLM_RECURSIVE_API_CALL`, and the list stands as it
/// was, as php's add and remove change `easyh` only on `CURLM_OK`.
///
/// ```php
/// $m = curl_multi_init();
/// $x = curl_init();
/// $b = curl_init('http://127.0.0.1:1/');
/// curl_setopt($b, CURLOPT_VERBOSE, 1);
/// curl_setopt($b, CURLOPT_DEBUGFUNCTION, function ($ch, $t, $d) use ($m, $b, $x) {
///     if (str_starts_with($d, 'closing')) {
///         var_dump(curl_multi_add_handle($m, $x), curl_multi_remove_handle($m, $b), count(curl_multi_get_handles($m)));
///     }
/// });
/// curl_multi_add_handle($m, $b);
/// curl_multi_exec($m, $r);
/// curl_multi_close($m);
/// var_dump(count(curl_multi_get_handles($m)));
/// ```
/// ```expect
/// int(8)
/// int(8)
/// int(1)
/// int(0)
/// ```
///
/// A fatal inside a remove leaves the list whole, as php's `zend_bailout`
/// leaves `easyh`, for `curl_multi_free_obj` to give up as the request
/// ends; an `exit` stands pending as php's unwind exit does, and the walk
/// goes on past it, removing every `CurlHandle` after it with its
/// callable serving nothing, before the list is emptied.
///
/// ```php
/// $m = curl_multi_init();
/// $h = curl_init('http://127.0.0.1:1/');
/// curl_setopt($h, CURLOPT_VERBOSE, 1);
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     if (str_starts_with($data, 'closing')) { echo "debug: $data"; trigger_error('boom', E_USER_ERROR); }
/// });
/// curl_multi_add_handle($m, $h);
/// curl_multi_add_handle($m, curl_init());
/// register_shutdown_function(function () use ($m) { var_dump(count(curl_multi_get_handles($m))); });
/// curl_multi_exec($m, $r);
/// curl_multi_close($m);
/// echo "unreached\n";
/// ```
/// ```expect exit=255
/// debug: closing connection #0
///
/// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 6
///
/// Fatal error: boom in %s on line 6
/// int(2)
/// ```
///
/// ```php
/// $m = curl_multi_init();
/// foreach ([1, 2] as $i) {
///     $h[$i] = curl_init('http://127.0.0.1:1/');
///     curl_setopt($h[$i], CURLOPT_VERBOSE, 1);
///     curl_setopt($h[$i], CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use ($i) {
///         if (str_starts_with($data, 'closing')) { echo "debug $i: $data"; if ($i === 1) { exit("exit\n"); } }
///     });
///     curl_multi_add_handle($m, $h[$i]);
/// }
/// register_shutdown_function(function () use ($m, $h) {
///     var_dump(count(curl_multi_get_handles($m)), curl_exec($h[2]), curl_errno($h[2]));
/// });
/// curl_multi_exec($m, $r);
/// curl_multi_close($m);
/// echo "unreached\n";
/// ```
/// ```expect
/// debug 1: closing connection #0
/// exit
/// debug 2: closing connection #0
/// int(0)
/// bool(false)
/// int(7)
/// ```
///
/// Called from a callable a `CurlHandle`'s own free serves, it passes
/// that `CurlHandle` over, whose cleanup detaches it, and lets `easyh`
/// go; the other `CurlHandle`, freed as `easyh` lets it go inside that
/// cleanup, prints nothing, under php as here:
///
/// ```php
/// $m = curl_multi_init();
/// $a = curl_init('http://127.0.0.1:1/');
/// $b = curl_init('http://127.0.0.1:1/');
/// foreach ([$a, $b] as $i => $h) {
///     curl_setopt($h, CURLOPT_VERBOSE, 1);
///     curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $t, $d) use ($m, $i) {
///         if ($t == CURLINFO_TEXT && str_starts_with($d, 'closing')) {
///             echo "$i: $d";
///             curl_multi_close($m);
///             var_dump(count(curl_multi_get_handles($m)));
///         }
///     });
///     curl_multi_add_handle($m, $h);
/// }
/// curl_multi_exec($m, $r);
/// unset($a, $b, $h, $m);
/// var_dump(gc_collect_cycles());
/// echo "end\n";
/// ```
/// ```expect
/// 0: closing connection #0
/// int(0)
/// int(5)
/// end
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
    let mut pending = Pending::default();
    // `zend_llist_get_next_ex`: the list read in place at each step. A
    // callable inside a remove adds to it or takes from it nothing, since
    // libcurl answers its add or remove on this multi
    // `CURLM_RECURSIVE_API_CALL`; a `curl_multi_close` it makes empties
    // it, which ends the walk.
    let mut at = 0;
    while let Some(handle) = listed(req, multi_handle, at) {
        at += 1;
        pending.step(req, |req, pending| {
            if let Some(thrown) = verify_handlers(req, handle, true) {
                pending.park(req, thrown);
            }
            // A `CurlHandle` whose free is under way has no `CURL *` to
            // remove: its cleanup, which served this callable, detaches it,
            // where php's remove is refused `CURLM_RECURSIVE_API_CALL`.
            if let Some(easy) = object_easy(req, handle)
                && let (_, Some(thrown)) = multi.remove(req, easy)
            {
                pending.park(req, thrown);
            }
        });
        if pending.fatal(req) {
            // `zend_bailout` leaves `easyh` whole, for
            // `curl_multi_free_obj` to give up as the request ends.
            return pending.answer(req, Ok(()));
        }
    }
    // `zend_llist_clean`, after every remove, with what stands pending.
    let mut handles = match mh(req, multi_handle) {
        Ok(mh) => std::mem::take(&mut mh.easy),
        Err(thrown) => return pending.answer(req, Err(thrown)),
    };
    pending.release(req, |req| {
        for handle in handles.drain(..) {
            handle.give_up(req);
        }
    });
    pending.answer(req, Ok(()))
}

/// The `CurlHandle` at `at` in `easyh`, read where it stands now.
fn listed(req: &Request, multi: Instance<CurlMultiHandle>, at: usize) -> Option<PhpObject> {
    let mh = multi.object().native::<Mh>(req)?;
    mh.easy.get(at).map(|held| **held)
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
/// then on and shares them as a copy does.
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
        easy.id().copied_from(cx.req, from);
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

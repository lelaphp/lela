//! `share.c`: the `CurlShareHandle` and `CurlSharePersistentHandle`
//! classes, each over a `CURLSH`, and their natives.

use std::ops::ControlFlow;

use php_ext::abi::{Instance, ObjectHandlers, Pool, Refusals};
use php_ext::attr::{php_class, php_function};
use php_ext::call::write_prop;
use php_ext::coerce::{to_long, try_long, value_name};
use php_ext::raising::{Thrown, argument_error, throw, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};

use crate::State;
use crate::constants as c;
use crate::ffi::{Persistent, Share, ShareRef, Shared, share_strerror};
use crate::handle::made;
use crate::throwables::Throwables;

/// `php_curlsh` less its `zend_object`, which both share classes lay.
#[derive(Default)]
pub(crate) struct Sh {
    /// `share`: the request keeps a `CurlShareHandle`'s, and this is its
    /// one owner; a persistent one is one owner of the pool's share.
    share: Option<Shared>,
    /// `err.no`.
    err: i64,
}

/// The share of an object of either share class, `None` for any other.
pub(crate) fn share_of(req: &Request, object: PhpObject) -> Option<ShareRef> {
    object
        .native::<Sh>(req)
        .and_then(|sh| sh.share.as_ref().map(Shared::as_ref))
}

/// `curl_share_handlers` and `curl_share_persistent_handlers`: what each
/// refuses, and `curl_share_free_obj`, which cleans a request's share up
/// and leaves a persistent one to the pool.
pub(crate) struct Handlers(Refusals);

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        self.0
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let share = object.native_mut::<Sh>(req).and_then(|sh| sh.share.take());
        if let Some(share) = share {
            share.close(req);
        }
    }
}

/// `final class CurlShareHandle`, `@strict-properties`,
/// `@not-serializable`.
///
/// ```php
/// try { new CurlShareHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = curl_share_init();
/// try { clone $s; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($s, $s == curl_share_init());
/// ```
/// ```expect
/// Cannot directly construct CurlShareHandle, use curl_share_init() instead
/// Trying to clone an uncloneable object of class CurlShareHandle
/// object(CurlShareHandle)#1 (0) {
/// }
/// bool(false)
/// ```
#[php_class(
    final,
    not_serializable,
    payload = Sh,
    handlers = SHARE_HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct CurlShareHandle;

#[php_class]
impl CurlShareHandle {}

/// `final class CurlSharePersistentHandle`, `@strict-properties`,
/// `@not-serializable`, with the options it was made with.
///
/// ```php
/// try { new CurlSharePersistentHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $s = curl_share_init_persistent([CURL_LOCK_DATA_CONNECT, CURL_LOCK_DATA_DNS, CURL_LOCK_DATA_DNS]);
/// var_dump($s);
/// try { $s->options = []; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot directly construct CurlSharePersistentHandle, use curl_share_init_persistent() instead
/// object(CurlSharePersistentHandle)#1 (1) {
///   ["options"]=>
///   array(2) {
///     [0]=>
///     int(3)
///     [1]=>
///     int(5)
///   }
/// }
/// Cannot modify readonly property CurlSharePersistentHandle::$options
/// ```
#[php_class(
    final,
    not_serializable,
    payload = Sh,
    handlers = PERSISTENT_HANDLERS,
    dynamic_properties = forbidden
)]
pub(crate) struct CurlSharePersistentHandle;

#[php_class]
impl CurlSharePersistentHandle {
    #[php(property, readonly)]
    const options: PhpArray;
}

/// `curl_share_handlers`: construction, cloning and comparison refused.
const SHARE: Refusals = Refusals::NONE
    .construction(Some("curl_share_init()"))
    .cloning()
    .comparison();

/// `curl_share_persistent_handlers`, which differ from
/// `curl_share_handlers` in the constructor's words.
const PERSISTENT: Refusals = SHARE.construction(Some("curl_share_init_persistent()"));

pub(crate) static SHARE_HANDLERS: Handlers = Handlers(SHARE);
pub(crate) static PERSISTENT_HANDLERS: Handlers = Handlers(PERSISTENT);

/// `curl_share_init(): CurlShareHandle`.
///
/// ```php
/// var_dump(curl_share_init() instanceof CurlShareHandle);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn curl_share_init(req: &mut Request) -> Result<Owner<Instance<CurlShareHandle>>, Thrown> {
    let object = made(req, b"CurlShareHandle")?;
    let share = Share::init(req);
    match object.native_mut::<Sh>(req) {
        Some(sh) => sh.share = share.map(Shared::Request),
        None => share.into_iter().for_each(|share| share.close(req)),
    }
    Ok(object.into_instance())
}

/// `curl_share_close(CurlShareHandle $share_handle): void`, which does
/// nothing.
///
/// ```php
/// curl_share_close(curl_share_init());
/// ```
/// ```expect
/// Deprecated: Function curl_share_close() is deprecated since 8.5, as it has no effect since PHP 8.0 in %s on line 2
/// ```
#[php_function(deprecated(since = "8.5", message = "as it has no effect since PHP 8.0"))]
fn curl_share_close(share_handle: Instance<CurlShareHandle>) {
    let _ = share_handle;
}

/// `curl_share_setopt(CurlShareHandle $share_handle, int $option, mixed $value): bool`.
///
/// ```php
/// $s = curl_share_init();
/// var_dump(curl_share_setopt($s, CURLSHOPT_SHARE, CURL_LOCK_DATA_COOKIE), curl_share_errno($s));
/// var_dump(curl_share_setopt($s, CURLSHOPT_SHARE, 99), curl_share_errno($s));
/// try { curl_share_setopt($s, 99, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// bool(false)
/// int(1)
/// curl_share_setopt(): Argument #2 ($option) is not a valid cURL share option
/// ```
#[php_function]
fn curl_share_setopt(
    req: &mut Request,
    share_handle: Instance<CurlShareHandle>,
    option: i64,
    value: Value,
) -> Result<bool, Thrown> {
    let object = share_handle.object();
    let (code, thrown) = match option {
        c::CURLSHOPT_SHARE | c::CURLSHOPT_UNSHARE => {
            let n = to_long(req, &value.deref())?;
            let code = share_of(req, object).map_or(1, |share| share.set_long(req, option, n));
            (code, None)
        }
        _ => {
            let ce = Throwables::of(req)?;
            (
                1,
                Some(argument_error(
                    req,
                    ce.value_error,
                    2,
                    "is not a valid cURL share option",
                )),
            )
        }
    };
    if let Some(sh) = object.native_mut::<Sh>(req) {
        sh.err = code;
    }
    match thrown {
        Some(thrown) => Err(thrown),
        None => Ok(code == 0),
    }
}

/// `curl_share_errno(CurlShareHandle $share_handle): int`.
///
/// ```php
/// var_dump(curl_share_errno(curl_share_init()));
/// ```
/// ```expect
/// int(0)
/// ```
#[php_function]
fn curl_share_errno(
    req: &mut Request,
    share_handle: Instance<CurlShareHandle>,
) -> Result<i64, Thrown> {
    Ok(share_handle.payload_mut(req)?.err)
}

/// `curl_share_strerror(int $error_code): ?string`.
///
/// ```php
/// var_dump(curl_share_strerror(0), curl_share_strerror(1), curl_share_strerror(-1));
/// ```
/// ```expect
/// string(8) "No error"
/// string(20) "Unknown share option"
/// string(18) "CURLSHcode unknown"
/// ```
#[php_function]
fn curl_share_strerror(
    req: &mut Request,
    error_code: i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    match share_strerror(error_code) {
        Some(text) => PhpString::new(req, &text).map(Some),
        None => Ok(None),
    }
}

/// `CURL_G(persistent_curlsh)` as the engine keeps it (design §4.10,
/// #1172): the persistent shares no request holds, each under the bits
/// of the options it was made with, lent to one request at a time.
pub(crate) struct Pooled {
    pub(crate) pool: Pool<u8, Persistent>,
    /// The classes the natives raise, resolved at the same start.
    pub(crate) throwables: Throwables,
}

/// The idle shares the pool keeps. php keeps one per set of options in
/// each php-fpm worker and bounds them by nothing else; this is the
/// bound the engine keeps for its own persistent streams.
pub(crate) const POOLED: usize = 1024;

/// The persistent share made with the options `id` names that the
/// request holds already, or one the engine's pool kept from an earlier
/// request, held by this one from here to its end.
fn lend(req: &mut Request, id: u8) -> Option<Persistent> {
    let held = &mut req.state::<State>().persistent;
    if let Some(at) = held.find(&id) {
        return held.get_mut(at).map(|share| share.clone());
    }
    let share = req.engine::<Pooled>()?.pool.take(&id)?;
    req.state::<State>().persistent.hold(id, share.clone());
    Some(share)
}

/// What php's `CURL_G(persistent_curlsh)` keeps across requests, at
/// `post_deactivate`, once the request's objects are freed: every
/// persistent share the request held given back to the engine's pool for
/// the next request asking the same options. A share past the pool's
/// bound is cleaned up here, with the ending request, and one a `CURL *`
/// or an object still holds is left to its holder, never lent to another
/// request while anything of this one could reach it (ADR-0010's
/// amendment of ADR-0005).
pub(crate) fn give_back(req: &mut Request) {
    let held = req.state::<State>().persistent.drain();
    for (id, share) in held {
        let past = match (share.alone(), req.engine::<Pooled>()) {
            (Some(share), Some(Pooled { pool, .. })) => pool.give_back(id, share),
            (share, _) => share,
        };
        drop(past);
    }
}

/// The data a persistent share holds, in the order its `options` lists
/// them, each with its bit of the pool's key.
const LOCKS: [(i64, u8); 4] = [
    (c::CURL_LOCK_DATA_DNS, 1),
    (c::CURL_LOCK_DATA_SSL_SESSION, 2),
    (c::CURL_LOCK_DATA_CONNECT, 4),
    (c::CURL_LOCK_DATA_PSL, 8),
];

/// `curl_share_init_persistent(array $share_options): CurlSharePersistentHandle`:
/// the share the request holds for the same options, or one the engine's
/// pool kept from an earlier request, or a new one, held by the request
/// for the next ask and given back to the pool as it ends.
///
/// ```php
/// $a = curl_share_init_persistent([CURL_LOCK_DATA_DNS]);
/// $b = curl_share_init_persistent([CURL_LOCK_DATA_DNS, CURL_LOCK_DATA_DNS]);
/// var_dump($a->options === $b->options);
/// foreach ([[], [CURL_LOCK_DATA_COOKIE], [30], ['x']] as $o) {
///     try { curl_share_init_persistent($o); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bool(true)
/// ValueError: curl_share_init_persistent(): Argument #1 ($share_options) must not be empty
/// ValueError: curl_share_init_persistent(): Argument #1 ($share_options) must not contain CURL_LOCK_DATA_COOKIE because sharing cookies across PHP requests is unsafe
/// ValueError: curl_share_init_persistent(): Argument #1 ($share_options) must contain only CURL_LOCK_DATA_* constants
/// TypeError: curl_share_init_persistent(): Argument #1 ($share_options) must contain only int values, string given
/// ```
///
/// Each option is read as `zval_try_get_long` reads it
/// ([`try_long`]): a leading-numeric string with php's warning, a
/// fractional float with its deprecation, where `curl_share_setopt`
/// reads its value silently, as `zval_get_long`.
///
/// ```php
/// $s = curl_share_init();
/// var_dump(curl_share_setopt($s, CURLSHOPT_SHARE, "3abc"));
/// var_dump(curl_share_init_persistent(["3abc", 3.0])->options);
/// try { curl_share_init_persistent(["2abc"]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_share_init_persistent([3.5]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_share_init_persistent(["x"]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
///
/// Warning: A non-numeric value encountered in %s on line 4
/// array(1) {
///   [0]=>
///   int(3)
/// }
///
/// Warning: A non-numeric value encountered in %s on line 5
/// curl_share_init_persistent(): Argument #1 ($share_options) must not contain CURL_LOCK_DATA_COOKIE because sharing cookies across PHP requests is unsafe
///
/// Deprecated: Implicit conversion from float 3.5 to int loses precision in %s on line 6
/// curl_share_init_persistent(): Argument #1 ($share_options) must contain only int values, string given
/// ```
#[php_function]
fn curl_share_init_persistent(
    req: &mut Request,
    share_options: PhpArray,
) -> Result<Owner<Instance<CurlSharePersistentHandle>>, Thrown> {
    let ce = Throwables::of(req)?;
    if share_options.is_empty() {
        return Err(argument_error(req, ce.value_error, 1, "must not be empty"));
    }
    let mut id = 0u8;
    // `ZEND_HASH_FOREACH_VAL(share_opts, zval *entry)`
    // (`Z/ext/curl/share.c:162`), no owner of the table taken: the
    // argument holds it while a `TypeError` reaches the program.
    walk::foreach(req, share_options, Hold::None, |req, row| -> walk::Step {
        let entry = row.value().deref();
        let Some(option) = try_long(req, &entry)? else {
            let tail = [
                b"must contain only int values, ".as_slice(),
                &value_name(req, &entry),
                b" given",
            ]
            .concat();
            return Err(argument_error(req, ce.type_error, 1, &tail));
        };
        if option == c::CURL_LOCK_DATA_COOKIE {
            return Err(argument_error(
                req,
                ce.value_error,
                1,
                "must not contain CURL_LOCK_DATA_COOKIE because sharing cookies across PHP requests is unsafe",
            ));
        }
        let Some((_, bit)) = LOCKS.iter().find(|(lock, _)| *lock == option) else {
            return Err(argument_error(
                req,
                ce.value_error,
                1,
                "must contain only CURL_LOCK_DATA_* constants",
            ));
        };
        id |= bit;
        Ok(ControlFlow::Continue(()))
    })?;
    let object = made(req, b"CurlSharePersistentHandle")?;
    let mut options = PhpArray::new(req)?;
    for (lock, bit) in LOCKS {
        if id & bit != 0 {
            options.append(req, Owner::int(lock))?;
        }
    }
    if let Err(thrown) = write_prop(req, &object, b"options", options.into_value()) {
        object.give_up(req);
        return Err(thrown);
    }
    let share = if let Some(share) = lend(req, id) {
        share
    } else {
        let Some(share) = Persistent::init() else {
            object.give_up(req);
            return Err(throw(
                req,
                ce.exception,
                "Could not construct persistent cURL share handle: out of memory",
            ));
        };
        for (lock, bit) in LOCKS {
            let code = if id & bit != 0 {
                share.set_long(c::CURLSHOPT_SHARE, lock)
            } else {
                0
            };
            if code != 0 {
                object.give_up(req);
                let text = format!(
                    "Could not construct persistent cURL share handle: {}",
                    String::from_utf8_lossy(&share_strerror(code).unwrap_or_default())
                );
                return Err(throw_exception_ex(req, ce.exception, &text));
            }
        }
        req.state::<State>().persistent.hold(id, share.clone());
        share
    };
    if let Some(sh) = object.native_mut::<Sh>(req) {
        sh.share = Some(Shared::Persistent(share));
    }
    Ok(object.into_instance())
}

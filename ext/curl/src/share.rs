//! `share.c`: the `CurlShareHandle` and `CurlSharePersistentHandle`
//! classes, each over a `CURLSH`, and their natives.

use php_ext::abi::{Instance, Refusals};
use php_ext::attr::{php_class, php_function};
use php_ext::call::write_prop;
use php_ext::coerce::{numeric_string, to_long, value_name};
use php_ext::raising::{Thrown, argument_error, throw};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::State;
use crate::constants as c;
use crate::ffi::{Share, share_strerror};
use crate::handle::made;

/// `php_curlsh` less its `zend_object`, which both share classes lay.
#[derive(Default)]
pub(crate) struct Sh {
    share: Option<Share>,
    /// `err.no`.
    err: i64,
}

/// The share of an object of either share class, `None` for any other.
pub(crate) fn share_of(req: &Request, object: PhpObject) -> Option<Share> {
    object.native::<Sh>(req).and_then(|sh| sh.share.clone())
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
    handlers = SHARE,
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
    handlers = PERSISTENT,
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
    if let Some(sh) = object.native_mut::<Sh>(req) {
        sh.share = Share::init();
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
            let code = share_of(req, object).map_or(1, |share| share.set_long(option, n));
            (code, None)
        }
        _ => (
            1,
            Some(argument_error(
                req,
                "ValueError",
                2,
                "is not a valid cURL share option",
            )),
        ),
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

/// `zval_try_get_long`: the integer a value is, `None` where it is none.
fn try_long(req: &mut Request, value: &Value) -> Result<Option<i64>, Thrown> {
    Ok(match value.as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Null | ValueRef::Bool(_) | ValueRef::Float(_) => Some(to_long(req, value)?),
        ValueRef::Str(text) => match numeric_string(text.as_bytes()) {
            Some(number) => Some(to_long(req, &number)?),
            None => None,
        },
        _ => None,
    })
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
/// the share the same options made before, or a new one kept for the
/// next ask.
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
#[php_function]
fn curl_share_init_persistent(
    req: &mut Request,
    share_options: PhpArray,
) -> Result<Owner<Instance<CurlSharePersistentHandle>>, Thrown> {
    if share_options.is_empty() {
        return Err(argument_error(req, "ValueError", 1, "must not be empty"));
    }
    let mut id = 0u8;
    for (_, entry) in share_options.snapshot() {
        let Some(option) = try_long(req, &entry)? else {
            let tail = format!(
                "must contain only int values, {} given",
                value_name(req, &entry)
            );
            return Err(argument_error(req, "TypeError", 1, &tail));
        };
        if option == c::CURL_LOCK_DATA_COOKIE {
            return Err(argument_error(
                req,
                "ValueError",
                1,
                "must not contain CURL_LOCK_DATA_COOKIE because sharing cookies across PHP requests is unsafe",
            ));
        }
        let Some((_, bit)) = LOCKS.iter().find(|(lock, _)| *lock == option) else {
            return Err(argument_error(
                req,
                "ValueError",
                1,
                "must contain only CURL_LOCK_DATA_* constants",
            ));
        };
        id |= bit;
    }
    let object = made(req, b"CurlSharePersistentHandle")?;
    let mut options = PhpArray::new(req)?;
    for (lock, bit) in LOCKS {
        if id & bit != 0 {
            options.append(req, Value::int(lock))?;
        }
    }
    if let Err(thrown) = write_prop(req, &object, b"options", options.into_value()) {
        object.give_up(req);
        return Err(thrown);
    }
    let kept = req
        .state::<State>()
        .persistent
        .iter()
        .find(|(held, _)| *held == id)
        .map(|(_, share)| share.clone());
    let share = if let Some(share) = kept {
        share
    } else {
        {
            let Some(share) = Share::init() else {
                object.give_up(req);
                return Err(throw(
                    req,
                    "Exception",
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
                    return Err(throw(req, "Exception", &text));
                }
            }
            req.state::<State>().persistent.push((id, share.clone()));
            share
        }
    };
    if let Some(sh) = object.native_mut::<Sh>(req) {
        sh.share = Some(share);
    }
    Ok(object.into_instance())
}

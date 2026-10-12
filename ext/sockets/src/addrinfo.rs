//! `AddressInfo` and the four `socket_addrinfo_*` natives over
//! `getaddrinfo(3)`: one object per entry of the list, holding the entry
//! as `php_addrinfo` copies it.

use std::ffi::CString;

use php_ext::abi::{Instance, ObjectHandlers, OrFalse, Refusals};
use php_ext::attr::{php_class, php_function};
use php_ext::coerce::{c_str, float_to_long, numeric_string, type_name};
use php_ext::raising::{Thrown, argument_error};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk};

use crate::State;
use crate::conv::put;
use crate::ffi::{self, Addr};
use crate::natives::{c_int, eth_proto, string};
use crate::socket::{Sock, Socket, made, socket_error, strerror};
use php_ext::raising::{Level, docref};

/// One `getaddrinfo` entry as `php_addrinfo` holds it; `None` only
/// while `socket_addrinfo_lookup` lays the object down. The entry, which
/// owns its address and its name, is boxed, inside the `sizeof` php
/// allocates (design §5.13).
#[derive(Default)]
pub(crate) struct Entry(Option<Held>);

/// The entry, with what `socket_addrinfo_lookup` `emalloc`s beside
/// `php_addrinfo` for it on the request's heap: `ai_addr`'s
/// `ai_addrlen` bytes, and `ai_canonname`'s `estrdup` where there is a
/// name, given back by `address_info_free_obj`.
///
/// ```php
/// $w = socket_addrinfo_lookup('127.0.0.1', '80', ['ai_family' => AF_INET]);
/// $m = memory_get_usage(); $a = socket_addrinfo_lookup('127.0.0.1', '80', ['ai_family' => AF_INET]); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = socket_addrinfo_lookup('::1', '80', ['ai_family' => AF_INET6]); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($a, $b); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 440
/// 472
/// -912
/// ```
pub(crate) struct Held {
    info: Box<ffi::AddrInfo>,
    addr: Emalloc,
    canonname: Option<Emalloc>,
}

impl Held {
    /// The entry and its two allocations; the `Err` is the memory limit.
    fn new(req: &mut Request, info: ffi::AddrInfo) -> Result<Held, Thrown> {
        let addr = Emalloc::new(req, info.addrlen)?;
        let canonname = match &info.canonname {
            Some(name) => Some(Emalloc::new(req, name.len() + 1)?),
            None => None,
        };
        Ok(Held {
            info: Box::new(info),
            addr,
            canonname,
        })
    }
}

/// `final class AddressInfo`, `@strict-properties`, `@not-serializable`.
///
/// ```php
/// try { new AddressInfo; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $a = socket_addrinfo_lookup('127.0.0.1')[0];
/// try { clone $a; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($a);
/// ```
/// ```expect
/// Cannot directly construct AddressInfo, use socket_addrinfo_lookup() instead
/// Trying to clone an uncloneable object of class AddressInfo
/// object(AddressInfo)#1 (0) {
/// }
/// ```
#[php_class(
    final,
    not_serializable,
    size = 104, payload = Entry,
    handlers = ADDRESS_INFO,
    dynamic_properties = forbidden
)]
pub(crate) struct AddressInfo;

#[php_class]
impl AddressInfo {}

/// `address_info_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static ADDRESS_INFO: Handlers = Handlers;

/// What the table refuses: construction, cloning and comparison.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("socket_addrinfo_lookup()"))
    .cloning()
    .comparison();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `address_info_free_obj`: `ai_canonname` and `ai_addr` given back.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let held = object
            .native_mut::<Entry>(req)
            .and_then(|entry| entry.0.take());
        if let Some(held) = held {
            if let Some(canonname) = held.canonname {
                canonname.free(req);
            }
            held.addr.free(req);
        }
    }
}

/// `zval_try_get_long`: a number, a numeric string, or `None`.
fn try_long(value: &Value) -> Option<i64> {
    match value.as_ref() {
        ValueRef::Int(n) => Some(n),
        ValueRef::Bool(b) => Some(i64::from(b)),
        ValueRef::Null => Some(0),
        ValueRef::Float(f) => Some(float_to_long(f)),
        ValueRef::Str(s) => match numeric_string(s.as_bytes())?.as_ref() {
            ValueRef::Int(n) => Some(n),
            ValueRef::Float(f) => Some(float_to_long(f)),
            _ => None,
        },
        _ => None,
    }
}

const HINT_KEYS: &str =
    "must only contain array keys \"ai_flags\", \"ai_socktype\", \"ai_protocol\", or \"ai_family\"";

/// `socket_addrinfo_lookup(string $host, ?string $service = null, array
/// $hints = []): array|false`: an `AddressInfo` for each entry
/// `getaddrinfo` answers, `false` where it answers none.
///
/// ```php
/// $r = socket_addrinfo_lookup('127.0.0.1', '80', ['ai_family' => AF_INET, 'ai_socktype' => SOCK_STREAM]);
/// var_dump(count($r), socket_addrinfo_explain($r[0]));
/// var_dump(socket_addrinfo_lookup('nonexistent.invalid'));
/// try { socket_addrinfo_lookup('x', null, [1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_addrinfo_lookup('x', null, ['ai_family' => 99]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_addrinfo_lookup('x', null, ['ai_flags' => 'x']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_addrinfo_lookup('x', null, ['ai_flags' => -1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// array(5) {
///   ["ai_flags"]=>
///   int(0)
///   ["ai_family"]=>
///   int(2)
///   ["ai_socktype"]=>
///   int(1)
///   ["ai_protocol"]=>
///   int(6)
///   ["ai_addr"]=>
///   array(2) {
///     ["sin_port"]=>
///     int(80)
///     ["sin_addr"]=>
///     string(9) "127.0.0.1"
///   }
/// }
/// bool(false)
/// socket_addrinfo_lookup(): Argument #3 ($hints) must only contain array keys "ai_flags", "ai_socktype", "ai_protocol", or "ai_family"
/// socket_addrinfo_lookup(): Argument #3 ($hints) "ai_family" key must be AF_INET or AF_INET6
/// socket_addrinfo_lookup(): Argument #3 ($hints) "ai_flags" key must be of type int, string given
/// socket_addrinfo_lookup(): Argument #3 ($hints) "ai_flags" key must be between 0 and 2147483647
/// ```
#[php_function]
fn socket_addrinfo_lookup(
    req: &mut Request,
    host: PhpString,
    #[php(default = null)] service: Option<PhpString>,
    #[php(default = [])] hints: PhpArray,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    if !hints.is_empty() && hints.is_list() {
        return Err(argument_error(req, "ValueError", 3, HINT_KEYS));
    }
    let want = match hints_wanted(req, hints) {
        Ok(want) => want,
        Err(refused) => return Err(refused.raise(req)),
    };
    let cut = |bytes: &[u8]| CString::new(c_str(bytes)).unwrap_or_default();
    let host = cut(host.as_bytes());
    let service = service.map(|s| cut(s.as_bytes()));
    let Ok(list) = ffi::getaddrinfo(&host, service.as_deref(), want) else {
        return Ok(OrFalse::False);
    };
    let mut out = PhpArray::new(req)?;
    for info in list {
        if info.family == libc::AF_UNSPEC {
            continue;
        }
        let object = made(req, b"AddressInfo")?;
        let held = Held::new(req, info)?;
        if let Some(entry) = object.native_mut::<Entry>(req) {
            entry.0 = Some(held);
        }
        out.append(req, object.into_value())?;
    }
    Ok(OrFalse::Value(out))
}

/// The four `hints` `getaddrinfo(3)` is handed, by key, read in one
/// walk; the `Err` is php's refusal of the first row that is not one,
/// raised once the walk has ended ([`Refused::raise`]).
fn hints_wanted(req: &Request, hints: PhpArray) -> Result<[i32; 4], Refused> {
    let mut want = [0i32; 4];
    for (key, hint) in walk::rows(req, hints) {
        let hint = hint.deref();
        let Key::Str(key) = key else {
            return Err(Refused::Keys);
        };
        let name = String::from_utf8_lossy(key.as_bytes()).into_owned();
        let slot = match name.as_str() {
            "ai_flags" => 0,
            "ai_family" => 1,
            "ai_socktype" => 2,
            "ai_protocol" => 3,
            _ => return Err(Refused::Keys),
        };
        let Some(n) = try_long(&hint) else {
            let given = type_name(req, &hint);
            return Err(Refused::NotInt { name, given });
        };
        if slot == 1 {
            if n != i64::from(libc::AF_INET) && n != i64::from(libc::AF_INET6) {
                return Err(Refused::Family);
            }
        } else if !(0..=i64::from(i32::MAX)).contains(&n) {
            return Err(Refused::Range { name });
        }
        want[slot] = i32::try_from(n).unwrap_or(0);
    }
    Ok(want)
}

/// Why [`hints_wanted`] refused a row of `hints`, read in the walk and
/// raised after it.
enum Refused {
    /// A key that is no hint's name.
    Keys,
    /// A hint whose value is no int, with its type's name.
    NotInt { name: String, given: String },
    /// An `ai_family` that is neither `AF_INET` nor `AF_INET6`.
    Family,
    /// A hint outside `0..=i32::MAX`.
    Range { name: String },
}

impl Refused {
    /// php's refusal of argument 3.
    fn raise(self, req: &mut Request) -> Thrown {
        match self {
            Refused::Keys => argument_error(req, "ValueError", 3, HINT_KEYS),
            Refused::NotInt { name, given } => {
                let tail = format!("\"{name}\" key must be of type int, {given} given");
                argument_error(req, "TypeError", 3, &tail)
            }
            Refused::Family => argument_error(
                req,
                "ValueError",
                3,
                "\"ai_family\" key must be AF_INET or AF_INET6",
            ),
            Refused::Range { name } => {
                let tail = format!("\"{name}\" key must be between 0 and {}", i32::MAX);
                argument_error(req, "ValueError", 3, &tail)
            }
        }
    }
}

/// The entry an `AddressInfo` holds, and an `InternalError` for one
/// that holds none, which only a lookup lays down.
fn entry(req: &mut Request, address: Instance<AddressInfo>) -> Result<ffi::AddrInfo, Thrown> {
    let held = address
        .payload_mut(req)?
        .0
        .as_ref()
        .map(|held| (*held.info).clone());
    held.ok_or_else(|| php_ext::raising::fatal(req, "InternalError: an AddressInfo holds no entry"))
}

/// `socket_addrinfo_bind` and `socket_addrinfo_connect`: a socket of
/// the entry's family, type and protocol, bound or connected to its
/// address.
fn open(
    req: &mut Request,
    address: Instance<AddressInfo>,
    connect: bool,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    let info = entry(req, address)?;
    let owner = made(req, b"Socket")?;
    let object = *owner;
    let protocol = eth_proto(i64::from(info.protocol), i64::from(info.family));
    let fd = match ffi::socket(info.family, info.socktype, c_int(protocol)) {
        Ok(fd) => fd,
        Err(errno) => {
            let code = errno as i32;
            req.state::<State>().last_error = code;
            let text = format!("Unable to create socket [{code}]: {}", strerror(req, code));
            docref(req, Level::Warning, &text);
            owner.give_up(req);
            return Ok(OrFalse::False);
        }
    };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    if let Some(sock) = object.native_mut::<Sock>(req) {
        *sock = Sock::owning(fd, info.family);
    }
    let (done, msg) = if connect {
        (ffi::connect(raw, &info.addr), "Unable to connect address")
    } else {
        (ffi::bind(raw, &info.addr), "Unable to bind address")
    };
    if let Err(errno) = done {
        socket_error(req, object, msg, errno as i32);
        owner.give_up(req);
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(owner.into_instance()))
}

/// `socket_addrinfo_connect(AddressInfo $address): Socket|false`.
///
/// ```php
/// $b = socket_addrinfo_bind(socket_addrinfo_lookup('127.0.0.1', '0', ['ai_family' => AF_INET, 'ai_socktype' => SOCK_DGRAM])[0]);
/// socket_getsockname($b, $ip, $port);
/// $c = socket_addrinfo_connect(socket_addrinfo_lookup('127.0.0.1', (string) $port, ['ai_family' => AF_INET, 'ai_socktype' => SOCK_DGRAM])[0]);
/// var_dump(socket_write($c, "hi"), socket_read($b, 10));
/// ```
/// ```expect
/// int(2)
/// string(2) "hi"
/// ```
#[php_function]
fn socket_addrinfo_connect(
    req: &mut Request,
    address: Instance<AddressInfo>,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    open(req, address, true)
}

/// `socket_addrinfo_bind(AddressInfo $address): Socket|false`.
///
/// ```php
/// $info = socket_addrinfo_lookup('127.0.0.1', '0', ['ai_family' => AF_INET, 'ai_socktype' => SOCK_STREAM])[0];
/// $s = socket_addrinfo_bind($info);
/// var_dump($s instanceof Socket, socket_getsockname($s, $a), $a);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(9) "127.0.0.1"
/// ```
#[php_function]
fn socket_addrinfo_bind(
    req: &mut Request,
    address: Instance<AddressInfo>,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    open(req, address, false)
}

/// `socket_addrinfo_explain(AddressInfo $address): array`: the entry's
/// fields and its address's.
///
/// ```php
/// $r = socket_addrinfo_lookup('::1', '8080', ['ai_family' => AF_INET6, 'ai_socktype' => SOCK_DGRAM]);
/// var_dump(socket_addrinfo_explain($r[0]));
/// ```
/// ```expect
/// array(5) {
///   ["ai_flags"]=>
///   int(0)
///   ["ai_family"]=>
///   int(30)
///   ["ai_socktype"]=>
///   int(2)
///   ["ai_protocol"]=>
///   int(17)
///   ["ai_addr"]=>
///   array(2) {
///     ["sin6_port"]=>
///     int(8080)
///     ["sin6_addr"]=>
///     string(3) "::1"
///   }
/// }
/// ```
#[php_function]
fn socket_addrinfo_explain(
    req: &mut Request,
    address: Instance<AddressInfo>,
) -> Result<Owner<PhpArray>, Thrown> {
    let info = entry(req, address)?;
    let mut out = PhpArray::new(req)?;
    put(req, &mut out, "ai_flags", Owner::int(i64::from(info.flags)))?;
    put(
        req,
        &mut out,
        "ai_family",
        Owner::int(i64::from(info.family)),
    )?;
    put(
        req,
        &mut out,
        "ai_socktype",
        Owner::int(i64::from(info.socktype)),
    )?;
    put(
        req,
        &mut out,
        "ai_protocol",
        Owner::int(i64::from(info.protocol)),
    )?;
    if let Some(name) = &info.canonname {
        let name = string(req, name)?;
        put(req, &mut out, "ai_canonname", name)?;
    }
    let mut addr = PhpArray::new(req)?;
    match &info.addr {
        Addr::V4(v4) if info.family == libc::AF_INET => {
            put(req, &mut addr, "sin_port", Owner::int(i64::from(v4.port())))?;
            let text = string(req, v4.ip().to_string().as_bytes())?;
            put(req, &mut addr, "sin_addr", text)?;
        }
        Addr::V6(v6) if info.family == libc::AF_INET6 => {
            put(
                req,
                &mut addr,
                "sin6_port",
                Owner::int(i64::from(v6.port())),
            )?;
            let text = string(req, v6.ip().to_string().as_bytes())?;
            put(req, &mut addr, "sin6_addr", text)?;
        }
        _ => {}
    }
    put(req, &mut out, "ai_addr", addr.into_value())?;
    Ok(out)
}

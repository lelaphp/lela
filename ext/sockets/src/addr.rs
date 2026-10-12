//! `sockaddr_conv.c`: an address as a program writes it, a dotted or
//! colon form or a host name, resolved to the one the socket's family
//! takes (`php_set_inet_addr`, `php_set_inet6_addr`,
//! `php_set_inet46_addr`); an address read back as text; and the
//! interface lookups `multicast.c` makes by name and by index.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

use nix::ifaddrs::getifaddrs;
use nix::net::if_::{if_indextoname, if_nametoindex};

use php_ext::coerce::c_str;
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::PhpObject;

use crate::ffi::{self, Addr};
use crate::socket::socket_error;

/// `MAXFQDNLEN`: the longest name `php_set_inet_addr` looks up.
const MAXFQDNLEN: usize = 255;

/// `HOST_NOT_FOUND`, `TRY_AGAIN`, `NO_RECOVERY` and `NO_DATA` of
/// `<netdb.h>`, the `h_errno` values the lookup's failures are.
const HOST_NOT_FOUND: i32 = 1;
const TRY_AGAIN: i32 = 2;
const NO_RECOVERY: i32 = 3;
const NO_DATA: i32 = 4;

/// `inet_pton(AF_INET, ..)`.
fn pton4(text: &[u8]) -> Option<Ipv4Addr> {
    std::str::from_utf8(c_str(text)).ok()?.parse().ok()
}

/// `inet_pton(AF_INET6, ..)`.
fn pton6(text: &[u8]) -> Option<Ipv6Addr> {
    std::str::from_utf8(c_str(text)).ok()?.parse().ok()
}

/// `php_network_gethostbyname` (`Z/main/network.c:1410`), which is
/// `gethostbyname(3)` on darwin: the host's first IPv4 address, or the
/// `h_errno` the lookup failed with. Asked of `getaddrinfo`, the one the
/// resolver answers on every thread, with its failures read back as
/// the `h_errno` `gethostbyname` sets for them.
fn gethostbyname(name: &[u8]) -> Result<Ipv4Addr, i32> {
    let Ok(host) = std::ffi::CString::new(c_str(name)) else {
        return Err(HOST_NOT_FOUND);
    };
    match ffi::getaddrinfo(&host, None, [0, libc::AF_INET, libc::SOCK_STREAM, 0]) {
        Ok(list) => list
            .into_iter()
            .find_map(|info| match info.addr {
                Addr::V4(v4) => Some(*v4.ip()),
                _ => None,
            })
            .ok_or(NO_DATA),
        Err(libc::EAI_NONAME) => Err(HOST_NOT_FOUND),
        Err(libc::EAI_AGAIN) => Err(TRY_AGAIN),
        #[cfg(target_os = "macos")]
        Err(libc::EAI_NODATA) => Err(NO_DATA),
        Err(_) => Err(NO_RECOVERY),
    }
}

/// `php_set_inet_addr` (`Z/ext/sockets/sockaddr_conv.c:88`): the
/// written address, or the host's; `None` after php's
/// `Host lookup failed` for a name that resolves to none, whose errno is
/// `-10000 - h_errno`.
pub(crate) fn inet_addr(req: &mut Request, socket: PhpObject, text: &[u8]) -> Option<Ipv4Addr> {
    if let Some(ip) = pton4(text) {
        return Some(ip);
    }
    let found = if text.len() > MAXFQDNLEN {
        Err(ffi::h_errno_now())
    } else {
        gethostbyname(text)
    };
    match found {
        Ok(ip) => Some(ip),
        Err(h) => {
            socket_error(req, socket, "Host lookup failed", -10000 - h);
            None
        }
    }
}

/// `php_set_inet6_addr` (`Z/ext/sockets/sockaddr_conv.c:15`): the
/// written address, or the host's through `getaddrinfo` with
/// `AI_V4MAPPED | AI_ADDRCONFIG`, then the scope after a `%`, a number
/// in `1..=UINT_MAX` or an interface's name; `None` after php's warning.
pub(crate) fn inet6_addr(
    req: &mut Request,
    socket: PhpObject,
    text: &[u8],
) -> Option<(Ipv6Addr, u32)> {
    let text = c_str(text);
    let ip = if let Some(ip) = pton6(text) {
        ip
    } else {
        let host = std::ffi::CString::new(text).ok()?;
        let hints = [
            libc::AI_V4MAPPED | libc::AI_ADDRCONFIG,
            libc::AF_INET6,
            0,
            0,
        ];
        let Ok(list) = ffi::getaddrinfo(&host, None, hints) else {
            socket_error(
                req,
                socket,
                "Host lookup failed",
                -10000 - ffi::h_errno_now(),
            );
            return None;
        };
        let Some(Addr::V6(v6)) = list.first().map(|info| &info.addr) else {
            docref(
                req,
                Level::Warning,
                "Host lookup failed: Non AF_INET6 domain returned on AF_INET6 socket",
            );
            return None;
        };
        *v6.ip()
    };
    let mut scope_id = 0;
    if let Some(at) = text.iter().position(|b| *b == b'%') {
        let scope = &text[at + 1..];
        match numeric_long(scope) {
            Some(n) => {
                if n > 0 && n <= i64::from(u32::MAX) {
                    scope_id = u32::try_from(n).unwrap_or(0);
                }
            }
            None => {
                if let Some(index) = if_index_named(req, scope) {
                    scope_id = index;
                }
            }
        }
    }
    Some((ip, scope_id))
}

/// `is_numeric_string(.., allow_errors = 0) == IS_LONG`: leading
/// whitespace, a sign and decimal digits, trailing whitespace allowed.
fn numeric_long(text: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(text).ok()?;
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let trimmed = trimmed.trim_end_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok()
}

/// `php_set_inet46_addr` (`Z/ext/sockets/sockaddr_conv.c:117`): the
/// address in the socket's own family; php's `ValueError` for a socket
/// of neither, and `None` after a lookup's warning.
pub(crate) fn inet46_addr(
    req: &mut Request,
    socket: PhpObject,
    family: i32,
    text: &[u8],
) -> Result<Option<Addr>, Thrown> {
    match family {
        libc::AF_INET => {
            Ok(inet_addr(req, socket, text).map(|ip| Addr::V4(SocketAddrV4::new(ip, 0))))
        }
        libc::AF_INET6 => Ok(inet6_addr(req, socket, text)
            .map(|(ip, scope)| Addr::V6(SocketAddrV6::new(ip, 0, 0, scope)))),
        _ => Err(throw_error(
            req,
            "ValueError",
            "IP address used in the context of an unexpected type of socket",
        )),
    }
}

/// `php_string_to_if_index` (`Z/ext/sockets/multicast.c:61`): the
/// interface's index by name, `None` after php's warning.
pub(crate) fn if_index_named(req: &mut Request, name: &[u8]) -> Option<u32> {
    let name = c_str(name);
    match if_nametoindex(name) {
        Ok(index) if index != 0 => Some(index),
        _ => {
            let text = format!(
                "No interface with name \"{}\" could be found",
                String::from_utf8_lossy(name)
            );
            docref(req, Level::Warning, &text);
            None
        }
    }
}

/// `php_if_index_to_addr4` (`Z/ext/sockets/multicast.c:709`): the
/// interface's IPv4 address, `INADDR_ANY` for index `0`, `None` after
/// php's warning. The address `SIOCGIFADDR` answers is the interface's
/// first of the family, which `getifaddrs` lists first.
pub(crate) fn if_index_to_addr4(req: &mut Request, index: u32) -> Option<Ipv4Addr> {
    if index == 0 {
        return Some(Ipv4Addr::UNSPECIFIED);
    }
    let failed = |req: &mut Request, errno: i32| {
        let text = format!("Failed obtaining address for interface {index}: error {errno}");
        docref(req, Level::Warning, &text);
        None
    };
    let name = match if_indextoname(index) {
        Ok(name) => name,
        Err(errno) => return failed(req, errno as i32),
    };
    let found = getifaddrs().ok().and_then(|mut list| {
        list.find_map(|entry| {
            let name_matches = entry.interface_name.as_bytes() == name.as_bytes();
            let v4 = entry.address.as_ref()?.as_sockaddr_in()?.ip();
            name_matches.then_some(v4)
        })
    });
    match found {
        Some(ip) => Some(ip),
        None => failed(req, libc::EADDRNOTAVAIL),
    }
}

/// `php_add4_to_if_index` (`Z/ext/sockets/multicast.c:748`): the index
/// of the interface holding `addr`, `0` for `INADDR_ANY`, `None` after
/// php's warning.
pub(crate) fn addr4_to_if_index(req: &mut Request, addr: Ipv4Addr) -> Option<u32> {
    if addr.is_unspecified() {
        return Some(0);
    }
    let name = getifaddrs().ok().and_then(|mut list| {
        list.find_map(|entry| {
            let ip = entry.address.as_ref()?.as_sockaddr_in()?.ip();
            (ip == addr).then_some(entry.interface_name)
        })
    });
    let Some(name) = name else {
        let text = format!("The interface with IP address {addr} was not found");
        docref(req, Level::Warning, &text);
        return None;
    };
    match if_nametoindex(name.as_str()) {
        Ok(index) => Some(index),
        Err(errno) => {
            let text = format!(
                "Error converting interface name to index: error {}",
                errno as i32
            );
            docref(req, Level::Warning, &text);
            None
        }
    }
}

/// `inet_ntop` of an address, as the natives hand it back.
pub(crate) fn ntop(addr: &Addr) -> Vec<u8> {
    match addr {
        Addr::V4(v4) => v4.ip().to_string().into_bytes(),
        Addr::V6(v6) => v6.ip().to_string().into_bytes(),
        Addr::Unix(path) => path.clone(),
        Addr::Other(_) => Vec::new(),
    }
}

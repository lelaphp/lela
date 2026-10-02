//! `socket_get_option` and `socket_set_option` with their aliases, and
//! `multicast.c`'s options under `IPPROTO_IP` and `IPPROTO_IPV6`: each
//! option php reads or writes as a structure is read or written as one,
//! and any other as a C `int`.

use std::net::Ipv4Addr;

use php_ext::abi::{Instance, OrFalse, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{to_long, to_string, truthy, value_name};
use php_ext::raising::{Thrown, arg_type_error, arg_value_error, argument_error, throw_error};
use php_ext::request::Request;
use php_ext::values::{PhpArray, Value, ValueRef};

use crate::addr::{addr4_to_if_index, if_index_named, if_index_to_addr4, inet46_addr};
use crate::conv::{Conv, find, put};
use crate::ffi::{self, Addr};
use crate::natives::c_int;
use crate::socket::{Socket, sock, socket_error, valid};

/// What `socket_get_option` answers: a structure's fields, or an `int`.
type Answer = OrFalse<Union<PhpArray, i64>>;

/// `struct timeval` as the platform lays it out.
fn timeval_bytes(sec: i64, usec: i64) -> Vec<u8> {
    let tv = libc::timeval {
        tv_sec: libc::time_t::try_from(sec).unwrap_or(libc::time_t::MAX),
        tv_usec: libc::suseconds_t::try_from(usec).unwrap_or(libc::suseconds_t::MAX),
    };
    let mut out = Vec::new();
    out.extend_from_slice(&tv.tv_sec.to_ne_bytes());
    out.extend_from_slice(&tv.tv_usec.to_ne_bytes());
    out.resize(std::mem::size_of::<libc::timeval>(), 0);
    out
}

fn word(bytes: &[u8], at: usize) -> i64 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&bytes[at..at + 4]);
    i64::from(i32::from_ne_bytes(b))
}

fn wide(bytes: &[u8], at: usize, width: usize) -> i64 {
    if width == 8 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&bytes[at..at + 8]);
        i64::from_ne_bytes(b)
    } else {
        word(bytes, at)
    }
}

/// Whether `optname` is `SO_LINGER`, or `SO_LINGER_SEC` where there is
/// one.
fn is_linger(optname: i64) -> bool {
    #[cfg(target_os = "macos")]
    if optname == i64::from(libc::SO_LINGER_SEC) {
        return true;
    }
    optname == i64::from(libc::SO_LINGER)
}

/// `socket_get_option` and `socket_getopt`.
fn get_option(
    req: &mut Request,
    socket: Instance<Socket>,
    level: i64,
    option: i64,
) -> Result<Answer, Thrown> {
    let fd = valid(req, socket)?;
    let object = socket.object();
    let failed = |req: &mut Request, errno: nix::errno::Errno, msg: &str| {
        socket_error(req, object, msg, errno as i32);
        Ok(OrFalse::False)
    };
    let (lvl, opt) = (c_int(level), c_int(option));
    if level == i64::from(libc::IPPROTO_IP) && option == i64::from(libc::IP_MULTICAST_IF) {
        let mut buf = [0u8; 4];
        if let Err(errno) = ffi::getsockopt(fd, lvl, opt, &mut buf) {
            return failed(req, errno, "Unable to retrieve socket option");
        }
        return Ok(match addr4_to_if_index(req, Ipv4Addr::from(buf)) {
            Some(index) => OrFalse::Value(Union::B(i64::from(index))),
            None => OrFalse::False,
        });
    }
    if level == i64::from(libc::IPPROTO_IPV6) && option == i64::from(libc::IPV6_PKTINFO) {
        let mut buf = [0u8; 20];
        if let Err(errno) = ffi::getsockopt(fd, lvl, opt, &mut buf) {
            return failed(req, errno, "unable to get socket option");
        }
        let value = Conv::in6_pktinfo_value(req, &buf)?;
        return Ok(match value.as_ref() {
            ValueRef::Array(array) => OrFalse::Value(Union::A(*array)),
            _ => OrFalse::False,
        });
    }
    if level == i64::from(libc::SOL_SOCKET) {
        if is_linger(option) {
            let mut buf = [0u8; std::mem::size_of::<libc::linger>()];
            if let Err(errno) = ffi::getsockopt(fd, lvl, opt, &mut buf) {
                return failed(req, errno, "Unable to retrieve socket option");
            }
            let mut out = PhpArray::new(req)?;
            put(req, &mut out, "l_onoff", Value::int(word(&buf, 0)))?;
            put(req, &mut out, "l_linger", Value::int(word(&buf, 4)))?;
            return Ok(OrFalse::Value(Union::A(out)));
        }
        if option == i64::from(libc::SO_RCVTIMEO) || option == i64::from(libc::SO_SNDTIMEO) {
            let mut buf = [0u8; std::mem::size_of::<libc::timeval>()];
            if let Err(errno) = ffi::getsockopt(fd, lvl, opt, &mut buf) {
                return failed(req, errno, "Unable to retrieve socket option");
            }
            let sec_width = std::mem::size_of::<libc::time_t>();
            let usec_width = std::mem::size_of::<libc::suseconds_t>();
            let mut out = PhpArray::new(req)?;
            put(req, &mut out, "sec", Value::int(wide(&buf, 0, sec_width)))?;
            put(
                req,
                &mut out,
                "usec",
                Value::int(wide(&buf, sec_width, usec_width)),
            )?;
            return Ok(OrFalse::Value(Union::A(out)));
        }
    }
    let mut buf = [0u8; 4];
    let len = match ffi::getsockopt(fd, lvl, opt, &mut buf) {
        Ok(len) => len,
        Err(errno) => return failed(req, errno, "Unable to retrieve socket option"),
    };
    let value = if len == 1 {
        i64::from(buf[0])
    } else {
        word(&buf, 0)
    };
    Ok(OrFalse::Value(Union::B(value)))
}

/// `socket_get_option(Socket $socket, int $level, int $option):
/// array|int|false`.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump(socket_get_option($s, SOL_SOCKET, SO_TYPE), socket_get_option($s, SOL_SOCKET, SO_LINGER), socket_get_option($s, SOL_SOCKET, SO_RCVTIMEO));
/// var_dump(socket_get_option($s, SOL_SOCKET, 99999));
/// ```
/// ```expect
/// int(1)
/// array(2) {
///   ["l_onoff"]=>
///   int(0)
///   ["l_linger"]=>
///   int(0)
/// }
/// array(2) {
///   ["sec"]=>
///   int(0)
///   ["usec"]=>
///   int(0)
/// }
///
/// Warning: socket_get_option(): Unable to retrieve socket option [42]: Protocol not available in %s on line 4
/// bool(false)
/// ```
///
/// A stream client made with `socket.tcp_nodelay` has `TCP_NODELAY`
/// set, which the kernel reports as its bit (#936):
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $ctx = stream_context_create(['socket' => ['tcp_nodelay' => true]]);
/// $c = stream_socket_client('tcp://' . stream_socket_get_name($srv, false), $e, $es, 1, STREAM_CLIENT_CONNECT, $ctx);
/// $plain = stream_socket_client('tcp://' . stream_socket_get_name($srv, false));
/// var_dump(socket_get_option(socket_import_stream($c), SOL_TCP, TCP_NODELAY) != 0);
/// var_dump(socket_get_option(socket_import_stream($plain), SOL_TCP, TCP_NODELAY));
/// ```
/// ```expect
/// bool(true)
/// int(0)
/// ```
#[php_function(aliases(socket_getopt))]
fn socket_get_option(
    req: &mut Request,
    socket: Instance<Socket>,
    level: i64,
    option: i64,
) -> Result<Answer, Thrown> {
    get_option(req, socket, level, option)
}

/// The table an option's array or object reads, `Z_ARRVAL_P` or
/// `Z_OBJPROP_P`, and php's `TypeError` for anything else.
fn table(req: &mut Request, value: &Value, option: &str) -> Result<PhpArray, Thrown> {
    match value.as_ref() {
        ValueRef::Array(array) => Ok(*array),
        ValueRef::Object(object) => {
            let mut out = PhpArray::new(req)?;
            for (key, element) in object.entries(req)? {
                req.addref(&element);
                if let php_ext::values::Key::Str(s) = &key {
                    req.addref(&Value::string(*s));
                }
                out.set(req, key, element)?;
            }
            Ok(out)
        }
        _ => {
            let given = value_name(req, value);
            let tail =
                format!("be of type array when argument #3 ($option) is {option}, {given} given");
            Err(arg_type_error(req, 4, &tail))
        }
    }
}

/// A key php requires of an option's array, `must have key "name"`.
fn key(req: &mut Request, table: PhpArray, name: &str) -> Result<Value, Thrown> {
    match find(table, name) {
        Some(value) => Ok(value),
        None => Err(arg_value_error(req, 4, &format!("have key \"{name}\""))),
    }
}

/// `php_get_if_index_from_zval`: an index, or an interface's by name.
fn if_index(req: &mut Request, value: &Value) -> Result<Option<u32>, Thrown> {
    if let ValueRef::Int(n) = value.as_ref() {
        return match u32::try_from(n) {
            Ok(index) => Ok(Some(index)),
            Err(_) => Err(throw_error(
                req,
                "ValueError",
                "Index must be between 0 and 4294967295",
            )),
        };
    }
    let name = to_string(req, value)?;
    Ok(if_index_named(req, name.as_bytes()))
}

/// What a set answered: done, refused after php's warning or
/// `ValueError`, or not an option this layer takes.
enum Set {
    Done(bool),
    Pass,
}

/// `php_do_mcast_opt` over `MCAST_JOIN_GROUP` and `MCAST_LEAVE_GROUP`,
/// the only two where there is no RFC 3678 API: the group and the
/// interface, then `_php_mcast_join_leave`.
fn mcast(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: i32,
    level: i32,
    join: bool,
    value: &Value,
) -> Result<Set, Thrown> {
    let object = socket.object();
    let table = table(req, value, "MCAST_LEAVE_GROUP")?;
    let Some(group) = find(table, "group") else {
        return Err(throw_error(
            req,
            "ValueError",
            "No key \"group\" passed in optval",
        ));
    };
    let group = to_string(req, &group)?;
    let family = sock(req, socket)?.family;
    let Some(group) = inet46_addr(req, object, family, group.as_bytes())? else {
        return Ok(Set::Done(false));
    };
    let index = match find(table, "interface") {
        Some(value) => match if_index(req, &value)? {
            Some(index) => index,
            None => return Ok(Set::Done(false)),
        },
        None => 0,
    };
    let (optname, bytes) = match (&group, family) {
        (Addr::V4(v4), libc::AF_INET) => {
            let Some(interface) = if_index_to_addr4(req, index) else {
                return Ok(Set::Done(false));
            };
            let mut mreq = v4.ip().octets().to_vec();
            mreq.extend_from_slice(&interface.octets());
            let op = if join {
                libc::IP_ADD_MEMBERSHIP
            } else {
                libc::IP_DROP_MEMBERSHIP
            };
            (op, mreq)
        }
        (Addr::V6(v6), libc::AF_INET6) => {
            let mut mreq = v6.ip().octets().to_vec();
            mreq.extend_from_slice(&index.to_ne_bytes());
            let op = if join {
                IPV6_JOIN_GROUP
            } else {
                IPV6_LEAVE_GROUP
            };
            (op, mreq)
        }
        _ => {
            let which = if join {
                "MCAST_JOIN_GROUP"
            } else {
                "MCAST_LEAVE_GROUP"
            };
            let text = format!("Option {which} is inapplicable to this socket type");
            return Err(throw_error(req, "ValueError", &text));
        }
    };
    Ok(Set::Done(
        match ffi::setsockopt(fd, level, optname, &bytes) {
            Ok(()) => true,
            Err(errno) => {
                socket_error(req, object, "Unable to set socket option", errno as i32);
                false
            }
        },
    ))
}

/// `setsockopt(2)` of bytes an option was laid out as, php's
/// `Unable to set socket option` where it fails.
fn apply(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: i32,
    level: i32,
    option: i32,
    bytes: &[u8],
) -> bool {
    match ffi::setsockopt(fd, level, option, bytes) {
        Ok(()) => true,
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "Unable to set socket option",
                errno as i32,
            );
            false
        }
    }
}

/// `php_do_setsockopt_ip_mcast`.
fn ip_mcast(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: i32,
    option: i32,
    value: &Value,
) -> Result<Set, Thrown> {
    let level = libc::IPPROTO_IP;
    let bytes = match option {
        libc::IP_ADD_MEMBERSHIP => return mcast(req, socket, fd, level, true, value),
        libc::IP_DROP_MEMBERSHIP => return mcast(req, socket, fd, level, false, value),
        libc::IP_MULTICAST_IF => {
            let Some(index) = if_index(req, value)? else {
                return Ok(Set::Done(false));
            };
            let Some(addr) = if_index_to_addr4(req, index) else {
                return Ok(Set::Done(false));
            };
            addr.octets().to_vec()
        }
        libc::IP_MULTICAST_LOOP => vec![u8::from(truthy(req, value))],
        libc::IP_MULTICAST_TTL => {
            let ttl = to_long(req, value)?;
            let Ok(ttl) = u8::try_from(ttl) else {
                return Err(arg_value_error(req, 4, "be between 0 and 255"));
            };
            vec![ttl]
        }
        _ => return Ok(Set::Pass),
    };
    Ok(Set::Done(apply(req, socket, fd, level, option, &bytes)))
}

/// `php_do_setsockopt_ipv6_mcast`, then `php_do_setsockopt_ipv6_rfc3542`.
fn ipv6_mcast(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: i32,
    option: i32,
    value: &Value,
) -> Result<Set, Thrown> {
    let level = libc::IPPROTO_IPV6;
    #[cfg(target_os = "macos")]
    let (join, leave) = (libc::IP_ADD_MEMBERSHIP, libc::IP_DROP_MEMBERSHIP);
    #[cfg(not(target_os = "macos"))]
    let (join, leave) = (libc::MCAST_JOIN_GROUP, libc::MCAST_LEAVE_GROUP);
    let bytes = match option {
        _ if option == join => return mcast(req, socket, fd, level, true, value),
        _ if option == leave => return mcast(req, socket, fd, level, false, value),
        libc::IPV6_MULTICAST_IF => {
            let Some(index) = if_index(req, value)? else {
                return Ok(Set::Done(false));
            };
            index.to_ne_bytes().to_vec()
        }
        libc::IPV6_MULTICAST_LOOP => i32::from(truthy(req, value)).to_ne_bytes().to_vec(),
        libc::IPV6_MULTICAST_HOPS => {
            let hops = to_long(req, value)?;
            if !(-1..=255).contains(&hops) {
                return Err(arg_value_error(req, 4, "be between -1 and 255"));
            }
            c_int(hops).to_ne_bytes().to_vec()
        }
        libc::IPV6_PKTINFO => {
            let family = sock(req, socket)?.family;
            let mut conv = Conv::user("in6_pktinfo", socket.object(), family);
            let bytes = conv.in6_pktinfo(req, value)?;
            if conv.failed() {
                conv.report(req);
                return Ok(Set::Done(false));
            }
            bytes
        }
        _ => return Ok(Set::Pass),
    };
    Ok(Set::Done(apply(req, socket, fd, level, option, &bytes)))
}

/// A number of an option's array, `zval_get_long`, held to `0..=65535`.
fn ushort_key(req: &mut Request, table: PhpArray, name: &str, shown: &str) -> Result<i64, Thrown> {
    let value = key(req, table, name)?;
    let n = to_long(req, &value)?;
    if !(0..=65535).contains(&n) {
        let tail = format!("\"{shown}\" must be between 0 and 65535");
        return Err(argument_error(req, "ValueError", 4, &tail));
    }
    Ok(n)
}

/// `socket_set_option` and `socket_setopt`.
fn set_option(
    req: &mut Request,
    socket: Instance<Socket>,
    level: i64,
    option: i64,
    value: &Value,
) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    let (lvl, opt) = (c_int(level), c_int(option));
    let handled = if level == i64::from(libc::IPPROTO_IP) {
        ip_mcast(req, socket, fd, opt, value)?
    } else if level == i64::from(libc::IPPROTO_IPV6) {
        ipv6_mcast(req, socket, fd, opt, value)?
    } else {
        Set::Pass
    };
    if let Set::Done(done) = handled {
        return Ok(done);
    }
    let bytes = if is_linger(option) {
        let table = table(req, value, "SO_LINGER")?;
        key(req, table, "l_onoff")?;
        key(req, table, "l_linger")?;
        let onoff = ushort_key(req, table, "l_onoff", "l_onoff")?;
        let linger = ushort_key(req, table, "l_linger", "l_linger")?;
        let mut out = c_int(onoff).to_ne_bytes().to_vec();
        out.extend_from_slice(&c_int(linger).to_ne_bytes());
        out
    } else if option == i64::from(libc::SO_RCVTIMEO) || option == i64::from(libc::SO_SNDTIMEO) {
        let name = if option == i64::from(libc::SO_RCVTIMEO) {
            "SO_RCVTIMEO"
        } else {
            "SO_SNDTIMEO"
        };
        let table = table(req, value, name)?;
        let sec = key(req, table, "sec")?;
        let usec = key(req, table, "usec")?;
        let sec = to_long(req, &sec)?;
        let usec = to_long(req, &usec)?;
        timeval_bytes(sec, usec)
    } else if is_bindtodevice(option) {
        let ValueRef::Str(s) = value.as_ref() else {
            let given = value_name(req, value);
            let tail = format!(
                "be of type string when argument #3 ($option) is SO_BINDTODEVICE, {given} given"
            );
            return Err(arg_type_error(req, 4, &tail));
        };
        s.as_bytes().to_vec()
    } else {
        c_int(to_long(req, value)?).to_ne_bytes().to_vec()
    };
    Ok(apply(req, socket, fd, lvl, opt, &bytes))
}

/// `socket_set_option(Socket $socket, int $level, int $option, $value):
/// bool`.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump(socket_set_option($s, SOL_SOCKET, SO_REUSEADDR, 1), socket_get_option($s, SOL_SOCKET, SO_REUSEADDR) != 0);
/// var_dump(socket_set_option($s, SOL_SOCKET, SO_LINGER, ['l_onoff' => 1, 'l_linger' => 3]), socket_get_option($s, SOL_SOCKET, SO_LINGER));
/// var_dump(socket_set_option($s, SOL_SOCKET, SO_RCVTIMEO, ['sec' => 2, 'usec' => 5000]), socket_get_option($s, SOL_SOCKET, SO_RCVTIMEO));
/// try { socket_set_option($s, SOL_SOCKET, SO_LINGER, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_set_option($s, SOL_SOCKET, SO_LINGER, ['l_onoff' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_set_option($s, SOL_TCP, TCP_NODELAY, true), socket_get_option($s, SOL_TCP, TCP_NODELAY) != 0);
/// $u = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
/// var_dump(socket_set_option($u, IPPROTO_IP, IP_MULTICAST_TTL, 4), socket_get_option($u, IPPROTO_IP, IP_MULTICAST_TTL));
/// try { socket_set_option($u, IPPROTO_IP, IP_MULTICAST_TTL, 256); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_set_option($u, IPPROTO_IP, MCAST_JOIN_GROUP, ['group' => '224.0.0.251', 'interface' => 0]));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// array(2) {
///   ["l_onoff"]=>
///   int(1)
///   ["l_linger"]=>
///   int(3)
/// }
/// bool(true)
/// array(2) {
///   ["sec"]=>
///   int(2)
///   ["usec"]=>
///   int(5000)
/// }
/// socket_set_option(): Argument #4 ($value) must be of type array when argument #3 ($option) is SO_LINGER, int given
/// socket_set_option(): Argument #4 ($value) must have key "l_linger"
/// bool(true)
/// bool(true)
/// bool(true)
/// int(4)
/// socket_set_option(): Argument #4 ($value) must be between 0 and 255
/// bool(true)
/// ```
#[php_function(aliases(socket_setopt))]
fn socket_set_option(
    req: &mut Request,
    socket: Instance<Socket>,
    level: i64,
    option: i64,
    value: Untyped<Value>,
) -> Result<bool, Thrown> {
    set_option(req, socket, level, option, &value.0)
}

/// Whether `optname` is `SO_BINDTODEVICE`, where the platform has it.
fn is_bindtodevice(option: i64) -> bool {
    #[cfg(target_os = "macos")]
    if option == crate::constants::SO_BINDTODEVICE {
        return true;
    }
    let _ = option;
    false
}

/// `IPV6_JOIN_GROUP` and `IPV6_LEAVE_GROUP`, the names `multicast.c`
/// uses: macOS and the BSDs declare them, and glibc aliases them to
/// `IPV6_ADD_MEMBERSHIP` and `IPV6_DROP_MEMBERSHIP`, which is all the
/// `libc` crate carries for Linux.
#[cfg(any(target_os = "linux", target_os = "android"))]
const IPV6_JOIN_GROUP: libc::c_int = libc::IPV6_ADD_MEMBERSHIP;
#[cfg(any(target_os = "linux", target_os = "android"))]
const IPV6_LEAVE_GROUP: libc::c_int = libc::IPV6_DROP_MEMBERSHIP;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const IPV6_JOIN_GROUP: libc::c_int = libc::IPV6_JOIN_GROUP;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const IPV6_LEAVE_GROUP: libc::c_int = libc::IPV6_LEAVE_GROUP;

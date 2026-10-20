//! `conversions.c`: a program's arrays read into the structures
//! `sendmsg(2)`, `recvmsg(2)` and `IPV6_PKTINFO` take, and the ones the
//! kernel fills read back into arrays, each failure named by the path of
//! keys that led to it, `error converting user data (path: msghdr >
//! name > addr): ..`, and reported once as php's warning.
//!
//! The ancillary table of `sendrecvmsg.c` is [`Ancillary`]: the
//! `(level, type)` pairs the platform has, each with its data's size
//! and its two conversions.

use std::net::{Ipv6Addr, SocketAddrV4, SocketAddrV6};
use std::ops::ControlFlow;
use std::os::fd::{AsRawFd, OwnedFd};

use php_ext::coerce::{c_str, float_to_long, numeric_string, with_string};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Cast, FdStream, Request, Stream, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Owner, PhpArray, PhpObject, Value, ValueRef};

use crate::addr::{inet_addr, inet6_addr, ntop};
use crate::ffi::{self, Addr, SUN_PATH, cmsg};
use crate::natives::string;
use crate::socket::{Sock, Socket, new_socket, socket_error};

/// `MAX_USER_BUFF_SIZE` and `DEFAULT_BUFF_SIZE`.
pub(crate) const MAX_USER_BUFF_SIZE: usize = 100 * 1024 * 1024;
pub(crate) const DEFAULT_BUFF_SIZE: usize = 8192;

/// `sizeof(struct in6_pktinfo)`: the address and the interface index.
const IN6_PKTINFO: usize = 16 + 4;

/// `sizeof(struct ucred)`: the pid, the uid and the gid.
#[cfg(target_os = "linux")]
const UCRED: usize = 4 + 4 + 4;

/// One `ancillary_reg_entry`: a `(level, type)` pair the platform has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ancillary {
    /// `IPV6_PKTINFO`, a `struct in6_pktinfo`.
    PktInfo,
    /// `IPV6_HOPLIMIT` and `IPV6_TCLASS`, an `int`.
    Int,
    /// `SCM_RIGHTS`, a sequence of fds.
    Rights,
    /// `SCM_CREDENTIALS`, a `struct ucred`, where `SO_PASSCRED` is.
    #[cfg(target_os = "linux")]
    Credentials,
}

impl Ancillary {
    /// `get_ancillary_reg_entry(level, type)`.
    pub(crate) fn of(level: i32, ty: i32) -> Option<Ancillary> {
        match (level, ty) {
            (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) => Some(Ancillary::PktInfo),
            (libc::IPPROTO_IPV6, libc::IPV6_HOPLIMIT | libc::IPV6_TCLASS) => Some(Ancillary::Int),
            (libc::SOL_SOCKET, libc::SCM_RIGHTS) => Some(Ancillary::Rights),
            #[cfg(target_os = "linux")]
            (libc::SOL_SOCKET, libc::SCM_CREDENTIALS) => Some(Ancillary::Credentials),
            _ => None,
        }
    }

    /// `entry->size`.
    pub(crate) const fn size(self) -> usize {
        match self {
            Ancillary::PktInfo => IN6_PKTINFO,
            Ancillary::Int => 4,
            Ancillary::Rights => 0,
            #[cfg(target_os = "linux")]
            Ancillary::Credentials => UCRED,
        }
    }

    /// `entry->var_el_size`.
    pub(crate) const fn element(self) -> usize {
        match self {
            Ancillary::Rights => 4,
            _ => 0,
        }
    }
}

/// `ser_context` and `res_context`: the keys that led here, the first
/// failure, and the socket the conversion is for. A failure with no
/// message is `try_convert_to_string`'s, whose own error stands.
pub(crate) struct Conv {
    keys: Vec<String>,
    error: Option<String>,
    /// `"user"` reading a program's array, `"native"` writing one.
    what: &'static str,
    socket: PhpObject,
    family: i32,
    /// `KEY_FILL_SOCKADDR`: whether a name's fields are read, false for
    /// `recvmsg`'s, whose size alone matters.
    fill_sockaddr: bool,
}

impl Conv {
    fn new(top: &str, what: &'static str, socket: PhpObject, family: i32) -> Conv {
        Conv {
            keys: vec![top.to_owned()],
            error: None,
            what,
            socket,
            family,
            fill_sockaddr: true,
        }
    }

    /// A conversion from a program's value, `from_zval_run_conversions`.
    pub(crate) fn user(top: &str, socket: PhpObject, family: i32) -> Conv {
        Conv::new(top, "user", socket, family)
    }

    /// A conversion into a program's value, `to_zval_run_conversions`.
    pub(crate) fn native(top: &str, socket: PhpObject, family: i32) -> Conv {
        Conv::new(top, "native", socket, family)
    }

    pub(crate) fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// `do_from_to_zval_err`: the first failure, with the path.
    fn fail(&mut self, text: &str) {
        if self.error.is_some() {
            return;
        }
        let path = if self.keys.is_empty() {
            "unavailable".to_owned()
        } else {
            self.keys.join(" > ")
        };
        self.error = Some(format!(
            "error converting {} data (path: {path}): {text}",
            self.what
        ));
    }

    /// `err_msg_dispose`: the failure's warning, where it has a message.
    pub(crate) fn report(&mut self, req: &mut Request) {
        if let Some(text) = self.error.take() {
            docref(req, Level::Warning, &text);
        }
    }

    fn under<T>(&mut self, key: &str, f: impl FnOnce(&mut Conv) -> T) -> T {
        self.keys.push(key.to_owned());
        let out = f(self);
        self.keys.pop();
        out
    }

    // ------------------------------------------------------- from zval

    /// The table an aggregation reads, `Z_ARRVAL_P`: an array, or
    /// php's `expected an array here`.
    fn table(&mut self, container: &Value) -> Option<PhpArray> {
        if let ValueRef::Array(array) = container.as_ref() {
            return Some(array);
        }
        self.fail("expected an array here");
        None
    }

    /// `from_zval_integer_common`.
    fn integer(&mut self, req: &mut Request, value: &Value) -> Result<i64, Thrown> {
        match value.as_ref() {
            ValueRef::Int(n) => Ok(n),
            ValueRef::Float(f) => Ok(float_to_long(f)),
            ValueRef::Str(_) | ValueRef::Object(_) => with_string(req, value, |text| {
                match numeric_string(text).as_ref().map(Value::as_ref) {
                    Some(ValueRef::Int(n)) => n,
                    Some(ValueRef::Float(f)) => float_to_long(f),
                    _ => {
                        let shown = String::from_utf8_lossy(c_str(text)).into_owned();
                        self.fail(&format!(
                            "expected an integer, but got a non numeric string (possibly from a converted object): '{shown}'"
                        ));
                        0
                    }
                }
            }),
            _ => {
                self.fail(
                    "expected an integer, either of a PHP integer type or of a convertible type",
                );
                Ok(0)
            }
        }
    }

    /// `from_zval_write_int`.
    pub(crate) fn int(&mut self, req: &mut Request, value: &Value) -> Result<i32, Thrown> {
        let n = self.integer(req, value)?;
        if self.failed() {
            return Ok(0);
        }
        i32::try_from(n).or_else(|_| {
            self.fail("given PHP integer is out of bounds for a native int");
            Ok(0)
        })
    }

    /// `from_zval_write_uint32`.
    fn uint32(&mut self, req: &mut Request, value: &Value) -> Result<u32, Thrown> {
        let n = self.integer(req, value)?;
        if self.failed() {
            return Ok(0);
        }
        u32::try_from(n).or_else(|_| {
            self.fail("given PHP integer is out of bounds for an unsigned 32-bit integer");
            Ok(0)
        })
    }

    /// `from_zval_write_net_uint16`, in host order here.
    fn uint16(&mut self, req: &mut Request, value: &Value) -> Result<u16, Thrown> {
        let n = self.integer(req, value)?;
        if self.failed() {
            return Ok(0);
        }
        u16::try_from(n).or_else(|_| {
            self.fail("given PHP integer is out of bounds for an unsigned 16-bit integer");
            Ok(0)
        })
    }

    /// `from_zval_write_sa_family`.
    fn sa_family(&mut self, req: &mut Request, value: &Value) -> Result<(), Thrown> {
        let n = self.integer(req, value)?;
        if !self.failed() && libc::sa_family_t::try_from(n).is_err() {
            self.fail("given PHP integer is out of bounds for a sa_family_t value");
        }
        Ok(())
    }

    /// `from_zval_write_sin_addr`.
    fn sin_addr(&mut self, req: &mut Request, value: &Value) -> Result<std::net::Ipv4Addr, Thrown> {
        let text = with_string(req, value, <[u8]>::to_vec)?;
        Ok(inet_addr(req, self.socket, &text).unwrap_or_else(|| {
            self.fail(&format!(
                "could not resolve address '{}' to get an AF_INET address",
                String::from_utf8_lossy(c_str(&text))
            ));
            std::net::Ipv4Addr::UNSPECIFIED
        }))
    }

    /// `from_zval_write_sin6_addr`.
    fn sin6_addr(&mut self, req: &mut Request, value: &Value) -> Result<Ipv6Addr, Thrown> {
        let text = with_string(req, value, <[u8]>::to_vec)?;
        Ok(inet6_addr(req, self.socket, &text).map_or_else(
            || {
                self.fail(&format!(
                    "could not resolve address '{}' to get an AF_INET6 address",
                    String::from_utf8_lossy(c_str(&text))
                ));
                Ipv6Addr::UNSPECIFIED
            },
            |(ip, _)| ip,
        ))
    }

    /// Each of `fields` the array has, read under its key in order, and
    /// php's `The key '%s' is required` for a required one it lacks:
    /// `from_zval_write_aggregation`.
    fn fields(
        &mut self,
        req: &mut Request,
        array: PhpArray,
        fields: &[(&str, bool)],
        mut read: impl FnMut(&mut Conv, &mut Request, &str, &Value) -> Result<(), Thrown>,
    ) -> Result<(), Thrown> {
        for (name, required) in fields {
            if self.failed() {
                break;
            }
            match find(array, name) {
                Some(value) => {
                    self.keys.push((*name).to_owned());
                    let done = read(self, req, name, &value);
                    self.keys.pop();
                    done?;
                }
                None if *required => {
                    self.fail(&format!("The key '{name}' is required"));
                    break;
                }
                None => {}
            }
        }
        Ok(())
    }

    /// `from_zval_write_sockaddr_aux`: a name in the family its
    /// `family` key names, or the socket's.
    pub(crate) fn sockaddr(
        &mut self,
        req: &mut Request,
        container: &Value,
    ) -> Result<Option<Addr>, Thrown> {
        let Some(array) = self.table(container) else {
            return Ok(None);
        };
        let family = match find(array, "family") {
            Some(value) if !matches!(value.as_ref(), ValueRef::Null) => {
                let family = self.under("family", |c| c.int(req, &value))?;
                if self.failed() {
                    return Ok(None);
                }
                family
            }
            _ => self.family,
        };
        let fill = self.fill_sockaddr;
        match family {
            libc::AF_INET => {
                if self.family != libc::AF_INET && self.family != libc::AF_INET6 {
                    self.fail(&format!(
                        "the specified family (number {family}) is not supported on this socket"
                    ));
                    return Ok(None);
                }
                let mut v4 = SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, 0);
                if fill {
                    let fields = [("family", false), ("addr", false), ("port", false)];
                    self.fields(req, array, &fields, |c, req, name, value| {
                        match name {
                            "family" => c.sa_family(req, value)?,
                            "addr" => v4.set_ip(c.sin_addr(req, value)?),
                            _ => v4.set_port(c.uint16(req, value)?),
                        }
                        Ok(())
                    })?;
                }
                Ok(Some(Addr::V4(v4)))
            }
            libc::AF_INET6 => {
                if self.family != libc::AF_INET6 {
                    self.fail("the specified family (AF_INET6) is not supported on this socket");
                    return Ok(None);
                }
                let mut v6 = SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0);
                if fill {
                    let fields = [
                        ("family", false),
                        ("addr", false),
                        ("port", false),
                        ("flowinfo", false),
                        ("scope_id", false),
                    ];
                    self.fields(req, array, &fields, |c, req, name, value| {
                        match name {
                            "family" => c.sa_family(req, value)?,
                            "addr" => v6.set_ip(c.sin6_addr(req, value)?),
                            "port" => v6.set_port(c.uint16(req, value)?),
                            "flowinfo" => v6.set_flowinfo(c.uint32(req, value)?),
                            _ => v6.set_scope_id(c.uint32(req, value)?),
                        }
                        Ok(())
                    })?;
                }
                Ok(Some(Addr::V6(v6)))
            }
            libc::AF_UNIX => {
                if self.family != libc::AF_UNIX {
                    self.fail("the specified family (AF_UNIX) is not supported on this socket");
                    return Ok(None);
                }
                let mut path = Vec::new();
                if fill {
                    let fields = [("family", false), ("path", false)];
                    self.fields(req, array, &fields, |c, req, name, value| {
                        if name == "family" {
                            return c.sa_family(req, value);
                        }
                        with_string(req, value, |text| {
                            if text.is_empty() {
                                c.fail("the path is must not be empty");
                            } else if text.len() >= SUN_PATH {
                                c.fail(&format!(
                                    "the path is too long, the maximum permitted length is {}",
                                    SUN_PATH - 1
                                ));
                            } else {
                                path = c_str(text).to_vec();
                            }
                        })
                    })?;
                }
                Ok(Some(Addr::Unix(path)))
            }
            _ => {
                self.fail(
                    "the only families currently supported are AF_INET, AF_INET6 and AF_UNIX",
                );
                Ok(None)
            }
        }
    }

    /// `from_zval_write_ifindex`.
    fn ifindex(&mut self, req: &mut Request, value: &Value) -> Result<u32, Thrown> {
        if let ValueRef::Int(n) = value.as_ref() {
            return Ok(u32::try_from(n).unwrap_or_else(|_| {
                self.fail(&format!(
                    "the interface index cannot be negative or larger than {}; given {n}",
                    u32::MAX
                ));
                0
            }));
        }
        with_string(req, value, |name| {
            let name = c_str(name);
            match nix::net::if_::if_nametoindex(name) {
                Ok(index) if index != 0 => index,
                _ => {
                    self.fail(&format!(
                        "no interface with name \"{}\" could be found",
                        String::from_utf8_lossy(name)
                    ));
                    0
                }
            }
        })
    }

    /// `from_zval_write_in6_pktinfo`: the 20 bytes of an `in6_pktinfo`.
    pub(crate) fn in6_pktinfo(
        &mut self,
        req: &mut Request,
        container: &Value,
    ) -> Result<Vec<u8>, Thrown> {
        let mut out = vec![0u8; IN6_PKTINFO];
        let Some(array) = self.table(container) else {
            return Ok(out);
        };
        self.fields(
            req,
            array,
            &[("addr", true), ("ifindex", true)],
            |c, req, name, value| {
                if name == "addr" {
                    let ip = c.sin6_addr(req, value)?;
                    out[..16].copy_from_slice(&ip.octets());
                } else {
                    let index = c.ifindex(req, value)?;
                    out[16..].copy_from_slice(&index.to_ne_bytes());
                }
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// `from_zval_write_ucred`: a `struct ucred`, each of its three
    /// fields required, the pid a non-negative `pid_t` and the ids
    /// `uid_t`s.
    #[cfg(target_os = "linux")]
    fn ucred(&mut self, req: &mut Request, container: &Value) -> Result<Vec<u8>, Thrown> {
        let mut out = vec![0u8; UCRED];
        let Some(array) = self.table(container) else {
            return Ok(out);
        };
        let fields = [("pid", true), ("uid", true), ("gid", true)];
        self.fields(req, array, &fields, |c, req, name, value| {
            let n = c.integer(req, value)?;
            if c.failed() {
                return Ok(());
            }
            let (at, word) = if name == "pid" {
                match i32::try_from(n) {
                    Ok(pid) if pid >= 0 => (0, pid.to_ne_bytes()),
                    _ => {
                        c.fail("given PHP integer is out of bounds for a pid_t value");
                        return Ok(());
                    }
                }
            } else {
                let at = if name == "uid" { 4 } else { 8 };
                let Ok(id) = u32::try_from(n) else {
                    c.fail("given PHP integer is out of bounds for a uid_t value");
                    return Ok(());
                };
                (at, id.to_ne_bytes())
            };
            out[at..at + 4].copy_from_slice(&word);
            Ok(())
        })?;
        Ok(out)
    }

    /// `calculate_scm_rights_space` and `from_zval_write_fd_array`.
    fn fd_array(&mut self, req: &mut Request, value: &Value) -> Vec<u8> {
        let Some(array) = self.table(value) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut at = 0;
        // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(arr), elem)`
        // (`Z/ext/sockets/conversions.c:956`), no owner of the table taken:
        // the argument holds it while a stream's cast runs its wrapper.
        let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
            at += 1;
            let key = format!("element #{at}");
            let element = row.value().deref();
            let fd = self.under(&key, |c| c.fd_of(req, &element));
            if self.failed() {
                return Ok(ControlFlow::Break(()));
            }
            out.extend_from_slice(&fd.to_ne_bytes());
            Ok(ControlFlow::Continue(()))
        });
        match walked {
            // The body answers no throwable.
            Ok(_) | Err(_) => {}
        }
        out
    }

    /// `from_zval_write_fd_array_aux`: a socket's fd, or a
    /// stream's cast to one.
    fn fd_of(&mut self, req: &mut Request, element: &Value) -> i32 {
        match element.as_ref() {
            ValueRef::Object(object) if object.native::<Sock>(req).is_some() => {
                let fd = object.native::<Sock>(req).and_then(Sock::fd);
                fd.unwrap_or_else(|| {
                    self.fail("socket is already closed");
                    -1
                })
            }
            ValueRef::Resource(_) => {
                let Some(stream) = Stream::from_value(req, element) else {
                    self.fail("resource is not a stream");
                    return -1;
                };
                stream.cast(req, Cast::Fd, true).unwrap_or_else(|| {
                    self.fail("cast stream to file descriptor failed");
                    -1
                })
            }
            _ => {
                self.fail("expected a Socket object or a stream resource");
                -1
            }
        }
    }

    /// `from_zval_write_control_array`: the ancillary data laid out, each
    /// element `CMSG_SPACE` of its data.
    pub(crate) fn control(&mut self, req: &mut Request, value: &Value) -> Result<Vec<u8>, Thrown> {
        let mut out = Vec::new();
        let Some(array) = self.table(value) else {
            return Ok(out);
        };
        let mut at = 0;
        // `from_array_iterate`'s `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(arr), elem)`
        // (`Z/ext/sockets/conversions.c:228`), no owner of the table taken:
        // the argument holds it while a field's conversion runs
        // `__toString`.
        walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
            if self.failed() {
                return Ok(ControlFlow::Break(()));
            }
            let key = format!("element #{at}");
            at += 1;
            let element = row.value().deref();
            self.under(&key, |c| c.cmsghdr(req, &element, &mut out))?;
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(out)
    }

    /// `from_zval_write_control`: one `cmsghdr`, its level, type and data.
    fn cmsghdr(
        &mut self,
        req: &mut Request,
        element: &Value,
        out: &mut Vec<u8>,
    ) -> Result<(), Thrown> {
        let Some(array) = self.table(element) else {
            return Ok(());
        };
        let mut level = 0;
        self.fields(req, array, &[("level", false)], |c, req, _, v| {
            level = c.int(req, v)?;
            Ok(())
        })?;
        if self.failed() {
            return Ok(());
        }
        let mut ty = 0;
        self.fields(req, array, &[("type", false)], |c, req, _, v| {
            ty = c.int(req, v)?;
            Ok(())
        })?;
        if self.failed() {
            return Ok(());
        }
        let Some(entry) = Ancillary::of(level, ty) else {
            self.fail(&format!(
                "cmsghdr with level {level} and type {ty} not supported"
            ));
            return Ok(());
        };
        let data_len = if entry == Ancillary::Rights {
            let Some(data) = find(array, "data") else {
                self.fail("cmsghdr should have a 'data' element here");
                return Ok(());
            };
            match data.as_ref() {
                ValueRef::Array(fds) if fds.is_empty() => {
                    self.fail("expected at least one element in this array");
                    return Ok(());
                }
                ValueRef::Array(fds) => fds.len() * 4,
                _ => {
                    self.fail("expected an array here");
                    return Ok(());
                }
            }
        } else {
            entry.size()
        };
        let start = out.len();
        out.resize(start + cmsg::space(data_len), 0);
        cmsg::put_header(&mut out[start..], cmsg::len(data_len), level, ty);
        let mut data = vec![0u8; data_len];
        self.fields(req, array, &[("data", false)], |c, req, _, v| {
            let bytes = match entry {
                Ancillary::PktInfo => c.in6_pktinfo(req, v)?,
                Ancillary::Int => c.int(req, v)?.to_ne_bytes().to_vec(),
                Ancillary::Rights => c.fd_array(req, v),
                #[cfg(target_os = "linux")]
                Ancillary::Credentials => c.ucred(req, v)?,
            };
            let n = bytes.len().min(data.len());
            data[..n].copy_from_slice(&bytes[..n]);
            Ok(())
        })?;
        let at = start + cmsg::DATA_AT;
        out[at..at + data_len].copy_from_slice(&data);
        Ok(())
    }

    /// `from_zval_write_iov_array`: each element's string.
    pub(crate) fn iov(&mut self, req: &mut Request, value: &Value) -> Result<Vec<Vec<u8>>, Thrown> {
        let Some(array) = self.table(value) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        // `from_array_iterate` (`Z/ext/sockets/conversions.c:228`), no
        // owner of the table taken: the argument holds it while
        // `zval_try_get_string` runs `__toString`.
        walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
            out.push(with_string(req, row.value(), <[u8]>::to_vec)?);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(out)
    }

    /// `from_zval_write_msghdr_recv`'s `buffer_size`.
    fn buffer_size(&mut self, req: &mut Request, value: &Value) -> Result<usize, Thrown> {
        let n = self.integer(req, value)?;
        if self.failed() {
            return Ok(0);
        }
        match usize::try_from(n) {
            Ok(size) if size <= MAX_USER_BUFF_SIZE => Ok(size),
            _ => {
                self.fail(&format!(
                    "the buffer size must be between 1 and {MAX_USER_BUFF_SIZE}; given {n}"
                ));
                Ok(0)
            }
        }
    }

    /// `from_zval_write_msghdr_send`: the name, the buffers and the
    /// ancillary data of a message to send.
    pub(crate) fn msghdr_send(
        &mut self,
        req: &mut Request,
        message: PhpArray,
    ) -> Result<Send, Thrown> {
        let mut send = Send::default();
        let fields = [("name", false), ("iov", false), ("control", false)];
        self.fields(req, message, &fields, |c, req, name, value| {
            match name {
                "name" => send.name = c.sockaddr(req, value)?,
                "iov" => send.iov = c.iov(req, value)?,
                _ => send.control = c.control(req, value)?,
            }
            Ok(())
        })?;
        Ok(send)
    }

    /// `from_zval_write_msghdr_recv`: how much room a message to
    /// receive is given, its name's included.
    pub(crate) fn msghdr_recv(
        &mut self,
        req: &mut Request,
        message: PhpArray,
    ) -> Result<Recv, Thrown> {
        let mut recv = Recv {
            name: None,
            size: DEFAULT_BUFF_SIZE,
            controllen: 0,
        };
        self.fill_sockaddr = false;
        let fields = [
            ("name", false),
            ("buffer_size", false),
            ("controllen", true),
        ];
        self.fields(req, message, &fields, |c, req, name, value| {
            match name {
                "name" => {
                    recv.name = c.sockaddr(req, value)?.map(|addr| match addr {
                        Addr::V4(_) => std::mem::size_of::<libc::sockaddr_in>(),
                        Addr::V6(_) => std::mem::size_of::<libc::sockaddr_in6>(),
                        _ => std::mem::size_of::<libc::sockaddr_un>(),
                    });
                }
                "buffer_size" => recv.size = c.buffer_size(req, value)?,
                _ => {
                    let len = c.uint32(req, value)?;
                    if !c.failed() && len == 0 {
                        c.fail("controllen cannot be 0");
                    }
                    recv.controllen = usize::try_from(len).unwrap_or(0);
                }
            }
            Ok(())
        })?;
        self.fill_sockaddr = true;
        Ok(recv)
    }

    // --------------------------------------------------------- to zval

    /// `to_zval_read_sockaddr_aux`: `null` for no family, an array of
    /// the family's fields otherwise.
    fn sockaddr_value(&mut self, req: &mut Request, addr: &Addr) -> Result<Owner<Value>, Thrown> {
        let mut out = PhpArray::new(req)?;
        match addr {
            Addr::Other(0) => {
                out.give_up(req);
                return Ok(Owner::null());
            }
            Addr::V4(v4) => {
                put(
                    req,
                    &mut out,
                    "family",
                    Owner::int(i64::from(libc::AF_INET)),
                )?;
                let text = string(req, &ntop(addr))?;
                put(req, &mut out, "addr", text)?;
                put(req, &mut out, "port", Owner::int(i64::from(v4.port())))?;
            }
            Addr::V6(v6) => {
                put(
                    req,
                    &mut out,
                    "family",
                    Owner::int(i64::from(libc::AF_INET6)),
                )?;
                let text = string(req, &ntop(addr))?;
                put(req, &mut out, "addr", text)?;
                put(req, &mut out, "port", Owner::int(i64::from(v6.port())))?;
                put(
                    req,
                    &mut out,
                    "flowinfo",
                    Owner::int(i64::from(v6.flowinfo())),
                )?;
                put(
                    req,
                    &mut out,
                    "scope_id",
                    Owner::int(i64::from(v6.scope_id())),
                )?;
            }
            Addr::Unix(path) => {
                put(
                    req,
                    &mut out,
                    "family",
                    Owner::int(i64::from(libc::AF_UNIX)),
                )?;
                let text = string(req, path)?;
                put(req, &mut out, "path", text)?;
            }
            Addr::Other(family) => {
                self.fail(&format!(
                    "cannot read struct sockaddr with family {family}; not supported"
                ));
            }
        }
        Ok(out.into_value())
    }

    /// `to_zval_read_in6_pktinfo`.
    pub(crate) fn in6_pktinfo_value(
        req: &mut Request,
        data: &[u8],
    ) -> Result<Owner<PhpArray>, Thrown> {
        let mut out = PhpArray::new(req)?;
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&data[..16]);
        let text = string(req, Ipv6Addr::from(octets).to_string().as_bytes())?;
        put(req, &mut out, "addr", text)?;
        let mut index = [0u8; 4];
        index.copy_from_slice(&data[16..20]);
        put(
            req,
            &mut out,
            "ifindex",
            Owner::int(i64::from(u32::from_ne_bytes(index))),
        )?;
        Ok(out)
    }

    /// `to_zval_read_ucred`: the pid, the uid and the gid.
    #[cfg(target_os = "linux")]
    fn ucred_value(req: &mut Request, data: &[u8]) -> Result<Owner<PhpArray>, Thrown> {
        let word = |at: usize| [data[at], data[at + 1], data[at + 2], data[at + 3]];
        let mut out = PhpArray::new(req)?;
        let pid = i64::from(i32::from_ne_bytes(word(0)));
        put(req, &mut out, "pid", Owner::int(pid))?;
        let uid = i64::from(u32::from_ne_bytes(word(4)));
        put(req, &mut out, "uid", Owner::int(uid))?;
        let gid = i64::from(u32::from_ne_bytes(word(8)));
        put(req, &mut out, "gid", Owner::int(gid))?;
        Ok(out)
    }

    /// `to_zval_read_fd_array`: each fd received, a `Socket` for
    /// a socket and a stream for anything else.
    fn fd_array_value(&mut self, req: &mut Request, data: &[u8]) -> Result<Owner<Value>, Thrown> {
        let mut out = PhpArray::new(req)?;
        let fds: Vec<OwnedFd> = data
            .chunks_exact(4)
            .map(|b| ffi::adopt(i32::from_ne_bytes([b[0], b[1], b[2], b[3]])))
            .collect();
        for fd in fds {
            if self.failed() {
                break;
            }
            let raw = fd.as_raw_fd();
            let meta = match nix::sys::stat::fstat(&fd) {
                Ok(meta) => meta,
                Err(errno) => {
                    self.fail(&format!(
                        "error creating resource for received file descriptor {raw}: fstat() call failed with errno {}",
                        errno as i32
                    ));
                    break;
                }
            };
            let element = if meta.st_mode & libc::S_IFMT == libc::S_IFSOCK {
                match import_descriptor(req, fd)? {
                    Ok(socket) => socket.into_value(),
                    Err(errno) => {
                        self.fail(&format!(
                            "error getting protocol descriptor {raw}: getsockopt() call failed with errno {errno}"
                        ));
                        break;
                    }
                }
            } else {
                let stream = req.stream_from_fd(fd, FdStream::File { mode: b"rw" })?;
                stream.into_resource().into_value()
            };
            out.append(req, element)?;
        }
        Ok(out.into_value())
    }

    /// `to_zval_read_control`: one `cmsghdr` as its level, type and data.
    fn cmsghdr_value(&mut self, req: &mut Request, at: &[u8]) -> Result<Owner<Value>, Thrown> {
        let (length, level, ty) = cmsg::header(at);
        let mut out = PhpArray::new(req)?;
        put(req, &mut out, "level", Owner::int(i64::from(level)))?;
        put(req, &mut out, "type", Owner::int(i64::from(ty)))?;
        let data = self.under("data", |c| -> Result<Owner<Value>, Thrown> {
            let Some(entry) = Ancillary::of(level, ty) else {
                c.fail(&format!("cmsghdr with level {level} and type {ty} not supported"));
                return Ok(Owner::null());
            };
            if cmsg::len(entry.size()) > length {
                c.fail(&format!(
                    "the cmsghdr structure is unexpectedly small; expected a length of at least {}, but got {length}",
                    cmsg::len(entry.size())
                ));
                return Ok(Owner::null());
            }
            let body = &at[cmsg::DATA_AT.min(at.len())..length.min(at.len())];
            match entry {
                Ancillary::PktInfo => Conv::in6_pktinfo_value(req, body).map(Owner::into_value),
                Ancillary::Int => {
                    let mut b = [0u8; 4];
                    b.copy_from_slice(&body[..4]);
                    Ok(Owner::int(i64::from(i32::from_ne_bytes(b))))
                }
                Ancillary::Rights => c.fd_array_value(req, body),
                #[cfg(target_os = "linux")]
                Ancillary::Credentials => Conv::ucred_value(req, body).map(Owner::into_value),
            }
        })?;
        put(req, &mut out, "data", data)?;
        Ok(out.into_value())
    }

    /// `to_zval_read_msghdr`: the name, the ancillary data, the buffers
    /// and the flags of a message received.
    pub(crate) fn msghdr_value(
        &mut self,
        req: &mut Request,
        got: &ffi::Received,
        size: usize,
    ) -> Result<Owner<Value>, Thrown> {
        let mut out = PhpArray::new(req)?;
        let name = match &got.name {
            Some(addr) => self.under("name", |c| c.sockaddr_value(req, addr))?,
            None => Owner::null(),
        };
        put(req, &mut out, "name", name)?;
        let control = self.under("control", |c| c.control_value(req, &got.control))?;
        put(req, &mut out, "control", control)?;
        let mut iov = PhpArray::new(req)?;
        if got.count > 0 {
            let n = got.count.min(size);
            let bytes = string(req, &got.data[..n])?;
            iov.append(req, bytes)?;
        }
        put(req, &mut out, "iov", iov.into_value())?;
        put(req, &mut out, "flags", Owner::int(i64::from(got.flags)))?;
        Ok(out.into_value())
    }

    /// `to_zval_read_control_array`: each `cmsghdr`, `CMSG_FIRSTHDR` and
    /// `CMSG_NXTHDR` walked over what the kernel left.
    fn control_value(&mut self, req: &mut Request, control: &[u8]) -> Result<Owner<Value>, Thrown> {
        let mut out = PhpArray::new(req)?;
        let mut at = 0usize;
        let mut index = 1;
        while at + cmsg::HEADER <= control.len() && !self.failed() {
            let (length, _, _) = cmsg::header(&control[at..]);
            let key = format!("element #{index}");
            index += 1;
            let element = self.under(&key, |c| c.cmsghdr_value(req, &control[at..]))?;
            out.append(req, element)?;
            if length == 0 {
                break;
            }
            at += cmsg::align(length);
            if at + cmsg::align(cmsg::HEADER) > control.len() {
                break;
            }
        }
        Ok(out.into_value())
    }
}

/// A message to send, as its array came to.
#[derive(Default)]
pub(crate) struct Send {
    pub(crate) name: Option<Addr>,
    pub(crate) iov: Vec<Vec<u8>>,
    pub(crate) control: Vec<u8>,
}

/// The room a message to receive is given.
pub(crate) struct Recv {
    pub(crate) name: Option<usize>,
    pub(crate) size: usize,
    pub(crate) controllen: usize,
}

/// `zend_hash_str_find(ht, name)`, dereferenced.
pub(crate) fn find(array: PhpArray, name: &str) -> Option<Value> {
    array.find(name.as_bytes()).map(|v| v.deref())
}

/// `add_assoc_*`: one more entry under a string key.
pub(crate) fn put(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    name: &str,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    array.set_str(req, name.as_bytes(), value)
}

/// `socket_import_file_descriptor` (`Z/ext/sockets/sockets.c:2542`) over
/// an fd this process owns: a socket of the family its name has
/// and the blocking state its flags have, or the errno that failed.
pub(crate) fn import_descriptor(
    req: &mut Request,
    fd: OwnedFd,
) -> Result<Result<Owner<php_ext::abi::Instance<Socket>>, i32>, Thrown> {
    let raw = fd.as_raw_fd();
    let made = new_socket(req, Sock::owning(fd, libc::AF_UNSPEC))?;
    let object = made.object();
    let family = match ffi::family(raw) {
        Ok(family) => family,
        Err(errno) => {
            socket_error(req, object, "Unable to obtain socket family", errno as i32);
            made.give_up(req);
            return Ok(Err(errno as i32));
        }
    };
    let flags = match ffi::status_flags(raw) {
        Ok(flags) => flags,
        Err(errno) => {
            socket_error(req, object, "Unable to obtain blocking state", errno as i32);
            made.give_up(req);
            return Ok(Err(errno as i32));
        }
    };
    if let Some(sock) = object.native_mut::<Sock>(req) {
        sock.family = family;
        sock.blocking = !ffi::is_nonblocking(flags);
    }
    Ok(Ok(made))
}

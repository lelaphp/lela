//! The socket kinds and their `transports` (design §4.3 seam 5): php's
//! `main/streams/xp_socket.c` as one kind over the socket `std` opened,
//! labelled as the ops php picked for it (`tcp_socket`, `udp_socket`,
//! `unix_socket`, `udg_socket`, and `generic_socket` for a socket the
//! stream was handed), and `main/streams/transports.c`'s
//! `php_stream_xport_create`, which reads a `transport` off the front of a
//! name and connects or binds a socket of that kind. The four `transports`
//! are the four `PHP_MINIT(standard)` registers; the `ssl` family is
//! `ext/openssl`'s, which this engine does not have.
//!
//! A read of a blocking socket with a timeout waits for the socket as
//! `php_sock_stream_wait_for_data` does, through `poll(2)`, which is
//! `php_heap`'s, and records the timeout `stream_get_meta_data` reports;
//! a read of a socket that does not block answers what is there, and the
//! kind never waits for a write.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::time::Duration;

use php_heap::Readiness;

use super::{CastTarget, MetaData, Ops, PhpStream, StatBuf, stat_buf, strerror};
use crate::api::raising::{Level, Thrown, raise, warning};
use crate::api::request::{OptionAnswer, Request, Stream, StreamOption, Xport, XportFailure};
use crate::api::values::PhpString;

/// `STREAM_OOB` and `STREAM_PEEK`, the bits of `stream_socket_sendto`'s
/// and `stream_socket_recvfrom`'s `$flags`.
const STREAM_OOB: i64 = 1;
const STREAM_PEEK: i64 = 2;

/// `ETIMEDOUT`, `PHP_TIMEOUT_ERROR_VALUE`: what a connect or an accept
/// that ran out of time reports, as `strerror` names it on this host.
const ETIMEDOUT: i32 = 60;

/// `ENOTCONN`, what a read of a listening socket meets.
const ENOTCONN: i32 = 57;

/// `EMSGSIZE`, which `php_sockop_set_option`'s liveness check counts as
/// transient beside `EAGAIN`, as this host numbers it.
const EMSGSIZE: i32 = 40;

/// One operation of `PHP_STREAM_OPTION_XPORT_API` past connecting and
/// binding, which [`Request::xport_create`] does itself.
pub(crate) enum XportOp<'a> {
    /// `STREAM_XPORT_OP_ACCEPT`: the next client within `timeout`.
    Accept {
        timeout: Option<Duration>,
        want_peer: bool,
    },
    /// `STREAM_XPORT_OP_GET_NAME` or, with `peer`,
    /// `STREAM_XPORT_OP_GET_PEER_NAME`.
    Name { peer: bool },
    /// `STREAM_XPORT_OP_SEND`, to `to` where the socket takes one.
    Send {
        buf: &'a [u8],
        to: Option<SocketAddr>,
    },
    /// `STREAM_XPORT_OP_RECV`, leaving the bytes queued under `peek`.
    Recv {
        buf: &'a mut [u8],
        peek: bool,
        want_addr: bool,
    },
    /// `STREAM_XPORT_OP_SHUTDOWN`.
    Shutdown(Shutdown),
}

/// What a socket kind answered one [`XportOp`].
pub(crate) enum Xported {
    /// The client's kind and its address, or the error text.
    Accepted(Result<(Socket, Option<Vec<u8>>), String>),
    /// The address as `php_network_populate_name_from_sockaddr` writes
    /// it, `None` where the call failed.
    Name(Option<Vec<u8>>),
    /// How many bytes went, `None` for `-1` after php's warning.
    Sent(Option<usize>),
    /// How many bytes came and from where, `None` for `-1`.
    Received(Option<(usize, Option<Vec<u8>>)>),
    /// Whether `shutdown(2)` answered `0`.
    Shut(bool),
}

/// The socket `std` holds, one variant per shape php's `socket(2)` was
/// asked for, and the one a stream was handed.
enum Sock {
    Tcp(TcpStream),
    TcpServer(TcpListener),
    Udp(UdpSocket),
    Unix(UnixStream),
    UnixServer(UnixListener),
    Udg(UnixDatagram),
    /// A socket the stream was handed, whatever its domain and type;
    /// `UnixStream` is only the shape of its calls, `read(2)`,
    /// `write(2)` and `shutdown(2)`, which any socket takes.
    Generic(UnixStream),
}

impl Sock {
    fn fd(&self) -> BorrowedFd<'_> {
        match self {
            Sock::Tcp(s) => s.as_fd(),
            Sock::TcpServer(s) => s.as_fd(),
            Sock::Udp(s) => s.as_fd(),
            Sock::Unix(s) | Sock::Generic(s) => s.as_fd(),
            Sock::UnixServer(s) => s.as_fd(),
            Sock::Udg(s) => s.as_fd(),
        }
    }

    /// `recv(2)`.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => s.read(buf),
            Sock::Udp(s) => s.recv(buf),
            Sock::Unix(s) | Sock::Generic(s) => s.read(buf),
            Sock::Udg(s) => s.recv(buf),
            Sock::TcpServer(_) | Sock::UnixServer(_) => {
                Err(std::io::Error::from_raw_os_error(ENOTCONN))
            }
        }
    }

    /// `send(2)`.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => s.write(buf),
            Sock::Udp(s) => s.send(buf),
            Sock::Unix(s) | Sock::Generic(s) => s.write(buf),
            Sock::Udg(s) => s.send(buf),
            Sock::TcpServer(_) | Sock::UnixServer(_) => {
                Err(std::io::Error::from_raw_os_error(ENOTCONN))
            }
        }
    }

    /// `php_set_sock_blocking`: `O_NONBLOCK` cleared or set.
    fn set_blocking(&self, blocking: bool) -> std::io::Result<()> {
        let nonblocking = !blocking;
        match self {
            Sock::Tcp(s) => s.set_nonblocking(nonblocking),
            Sock::TcpServer(s) => s.set_nonblocking(nonblocking),
            Sock::Udp(s) => s.set_nonblocking(nonblocking),
            Sock::Unix(s) | Sock::Generic(s) => s.set_nonblocking(nonblocking),
            Sock::UnixServer(s) => s.set_nonblocking(nonblocking),
            Sock::Udg(s) => s.set_nonblocking(nonblocking),
        }
    }
}

/// `php_netstream_data_t` under one of `xp_socket.c`'s ops: the socket,
/// `None` once closed; the ops' label; `is_blocked`; the read timeout,
/// `None` for php's `tv_sec == -1`; `timeout_event`; and whether the
/// stream is a server's, `PHP_STREAM_FLAG_NO_IO`, which
/// `php_stream_xport_create` sets on a stream it bound
/// (`Z/main/streams/transports.c:175`) and which only this kind reads.
pub(crate) struct Socket {
    sock: Option<Sock>,
    label: &'static str,
    is_blocked: bool,
    timeout: Option<Duration>,
    timeout_event: bool,
    no_io: bool,
}

impl Socket {
    fn new(sock: Sock, label: &'static str, timeout: Option<Duration>) -> Socket {
        Socket {
            sock: Some(sock),
            label,
            is_blocked: true,
            timeout,
            timeout_event: false,
            no_io: false,
        }
    }

    /// `recv(2)` with `MSG_DONTWAIT` on a socket that blocks: the socket
    /// made non-blocking for the one call.
    fn read_now(sock: &mut Sock, buf: &mut [u8]) -> std::io::Result<usize> {
        sock.set_blocking(false)?;
        let read = sock.read(buf);
        let _ = sock.set_blocking(true);
        read
    }
}

/// A timeout of seconds as `default_socket_timeout` holds it: `None`, no
/// limit, for a negative one.
pub(crate) fn seconds(timeout: i64) -> Option<Duration> {
    u64::try_from(timeout).ok().map(Duration::from_secs)
}

/// `FG(default_socket_timeout)`, the ini entry's seconds, sixty where
/// the bundle declares none.
fn default_timeout(req: &Request) -> Option<Duration> {
    let secs = req.ini_get(b"default_socket_timeout").map_or(60, |text| {
        String::from_utf8_lossy(text).trim().parse().unwrap_or(0)
    });
    seconds(secs)
}

/// `php_network_populate_name_from_sockaddr` (`Z/main/network.c:645`):
/// an inet address as `ip:port`, `[ip]:port` for IPv6.
fn inet_text(addr: &SocketAddr) -> Vec<u8> {
    match addr {
        SocketAddr::V4(v4) => format!("{}:{}", v4.ip(), v4.port()).into_bytes(),
        SocketAddr::V6(v6) => format!("[{}]:{}", v6.ip(), v6.port()).into_bytes(),
    }
}

/// A unix socket's address as the same function writes it: the path,
/// and nothing for one with none.
fn unix_text(addr: &std::os::unix::net::SocketAddr) -> Vec<u8> {
    addr.as_pathname()
        .map(|path| path.as_os_str().as_bytes().to_vec())
        .unwrap_or_default()
}

/// `php_socket_error_str`: `strerror` of the `errno` an error carries,
/// and of `ETIMEDOUT` for `std`'s own timeout, which carries none.
fn socket_error(error: &std::io::Error) -> (i32, String) {
    if error.raw_os_error().is_none() && error.kind() == std::io::ErrorKind::TimedOut {
        let timed_out = std::io::Error::from_raw_os_error(ETIMEDOUT);
        return (ETIMEDOUT, strerror(&timed_out));
    }
    (error.raw_os_error().unwrap_or(0), strerror(error))
}

impl Ops for Socket {
    fn label(&self) -> &'static str {
        self.label
    }

    /// `php_sockop_write` (`Z/main/streams/xp_socket.c:63`): one
    /// `send(2)`, a socket that does not block answering `0` where it
    /// would, and php's notice with `errno` where it failed.
    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        let Some(sock) = self.sock.as_mut() else {
            return Some(0);
        };
        match sock.write(buf) {
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
            Err(error) => {
                let (code, text) = socket_error(&error);
                let text = format!(
                    "{}(): Send of {} bytes failed with errno={code} {text}",
                    req.active_native(),
                    buf.len()
                );
                raise(req, Level::Notice, &text);
                None
            }
        }
    }

    /// `php_sockop_read` (`Z/main/streams/xp_socket.c:170`): a socket that
    /// blocks waits for data up to its timeout, the wait that runs out
    /// setting `timeout_event` and answering `-1`, and one with a zero
    /// timeout does not wait; then `recv(2)`, `0` bytes where the socket
    /// would block, eof at the peer's close and at an error.
    fn read(&mut self, _req: &mut Request, buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        let sock = self.sock.as_mut()?;
        let mut dont_wait = false;
        if self.is_blocked {
            dont_wait = self.timeout == Some(Duration::ZERO);
            if !dont_wait {
                self.timeout_event = false;
                loop {
                    match php_heap::poll_fd(sock.fd(), Readiness::Readable, self.timeout) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Ok(true) | Err(_) => break,
                        Ok(false) => {
                            self.timeout_event = true;
                            return None;
                        }
                    }
                }
            }
        }
        let read = if dont_wait {
            Socket::read_now(sock, buf)
        } else {
            sock.read(buf)
        };
        match read {
            Ok(0) => {
                *eof = true;
                Some(0)
            }
            Ok(n) => Some(n),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Some(0),
            Err(_) => {
                *eof = true;
                None
            }
        }
    }

    /// `php_sockop_close`: `closesocket`, which the drop is.
    fn close(&mut self, _req: &mut Request) -> i32 {
        self.sock = None;
        0
    }

    /// `php_sockop_stat`: `fstat(2)` of the socket.
    fn stat(&mut self, _req: &mut Request) -> Option<StatBuf> {
        let fd = self.sock.as_ref()?.fd().try_clone_to_owned().ok()?;
        let meta = std::fs::File::from(fd).metadata().ok()?;
        Some(stat_buf(&meta))
    }

    fn meta_data(&self) -> MetaData {
        MetaData::Socket {
            timed_out: self.timeout_event,
            blocked: self.is_blocked,
        }
    }

    /// `php_sockop_cast`: the socket, for either cast.
    fn cast_target(
        &mut self,
        _req: &mut Request,
        _select: bool,
        _own: Option<Stream>,
    ) -> CastTarget<'_> {
        self.sock
            .as_ref()
            .map_or(CastTarget::Refused, |sock| CastTarget::Fd(sock.fd()))
    }

    /// `php_sockop_set_option` (`Z/main/streams/xp_socket.c:333`):
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` is [`Socket::alive`],
    /// `PHP_STREAM_OPTION_BLOCKING` is `php_set_sock_blocking`, and
    /// `PHP_STREAM_OPTION_READ_TIMEOUT` the timeout a read waits for.
    fn set_option(&mut self, _req: &mut Request, option: StreamOption) -> OptionAnswer {
        match option {
            StreamOption::Blocking(blocking) => {
                let Some(sock) = self.sock.as_ref() else {
                    return OptionAnswer::Err;
                };
                if sock.set_blocking(blocking).is_err() {
                    return OptionAnswer::Err;
                }
                self.is_blocked = blocking;
                OptionAnswer::Ok
            }
            StreamOption::ReadTimeout(timeout) => {
                self.timeout = Some(timeout);
                self.timeout_event = false;
                OptionAnswer::Ok
            }
            StreamOption::Liveness => {
                if self.alive() {
                    OptionAnswer::Ok
                } else {
                    OptionAnswer::Err
                }
            }
            _ => OptionAnswer::NotImplemented,
        }
    }

    fn xport(&mut self, req: &mut Request, op: XportOp<'_>) -> Option<Xported> {
        if let XportOp::Accept { timeout, want_peer } = op {
            return Some(Xported::Accepted(self.accept(timeout, want_peer)));
        }
        let sock = self.sock.as_mut()?;
        Some(match op {
            XportOp::Accept { .. } => return None,
            XportOp::Name { peer } => Xported::Name(name(sock, peer)),
            XportOp::Send { buf, to } => {
                let sent = match (sock, to) {
                    (Sock::Udp(s), Some(to)) => s.send_to(buf, to),
                    (sock, _) => sock.write(buf),
                };
                Xported::Sent(match sent {
                    Ok(n) => Some(n),
                    Err(error) => {
                        let text = format!("{}(): {}\n", req.active_native(), strerror(&error));
                        warning(req, &text);
                        None
                    }
                })
            }
            XportOp::Recv {
                buf,
                peek,
                want_addr,
            } => Xported::Received(recv(sock, buf, peek, want_addr)),
            XportOp::Shutdown(how) => Xported::Shut(match sock {
                Sock::Tcp(s) => s.shutdown(how).is_ok(),
                Sock::Unix(s) | Sock::Generic(s) => s.shutdown(how).is_ok(),
                Sock::Udg(s) => s.shutdown(how).is_ok(),
                _ => false,
            }),
        })
    }
}

impl Socket {
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` as `php_sockop_set_option`
    /// answers it (`Z/main/streams/xp_socket.c:342-386`) for the `0`
    /// that `php_stream_eof` and `php_stream_xport_create` ask with: a
    /// closed socket is dead; a server's stream (`PHP_STREAM_FLAG_NO_IO`)
    /// is polled for a readable socket without waiting and is alive where
    /// none is; then one byte is peeked without waiting, `MSG_PEEK |
    /// MSG_DONTWAIT`, and the peer's orderly close, which leaves none,
    /// is dead, as a failure other than `EAGAIN` or `EMSGSIZE` is. So
    /// `feof()` sees a peer that went away before any read finds the end,
    /// and a listening socket with a connection waiting, whose peek
    /// fails with `ENOTCONN`.
    ///
    /// Each `true` below is read after `stream_select` has seen the end or
    /// the connection arrive, since the check waits for nothing and the
    /// kernel hands a close or a connection to the other end when it
    /// does: read without the wait, a loaded machine answers `false` there
    /// now and then, under the oracle and this engine alike (#975). Each
    /// `false` holds whether or not anything has arrived yet.
    ///
    /// ```php
    /// $ready = function ($s) { $r = [$s]; $w = $e = null; stream_select($r, $w, $e, 10); };
    /// [$a, $b] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
    /// var_dump(feof($b));
    /// fwrite($a, "x"); fclose($a);
    /// var_dump(feof($b), fread($b, 10));
    /// $ready($b);
    /// var_dump(feof($b));
    /// [$c, $d] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
    /// stream_set_blocking($d, false);
    /// fclose($c);
    /// $ready($d);
    /// var_dump(feof($d));
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// var_dump(feof($s));
    /// $client = stream_socket_client(stream_socket_get_name($s, false));
    /// $ready($s);
    /// var_dump(feof($s), feof($client));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(1) "x"
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn alive(&self) -> bool {
        let Some(sock) = self.sock.as_ref() else {
            return false;
        };
        if self.no_io
            && !matches!(
                php_heap::poll_fd(sock.fd(), Readiness::Readable, Some(Duration::ZERO)),
                Ok(true)
            )
        {
            return true;
        }
        let mut byte = [0u8; 1];
        let peeked = if self.is_blocked {
            // `MSG_DONTWAIT` on a socket that blocks: made non-blocking
            // for the one call, as [`Socket::read_now`] makes it.
            let _ = sock.set_blocking(false);
            let peeked = php_heap::peek_fd(sock.fd(), &mut byte);
            let _ = sock.set_blocking(true);
            peeked
        } else {
            php_heap::peek_fd(sock.fd(), &mut byte)
        };
        match peeked {
            Ok(n) => n > 0,
            Err(error) => {
                error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(EMSGSIZE)
            }
        }
    }

    /// `php_tcp_sockop_accept` over `php_network_accept_incoming`
    /// (`Z/main/network.c:772`): the listener waited for up to `timeout`,
    /// `ETIMEDOUT` where it ran out, then `accept(2)`; the client a socket
    /// of the listener's kind, blocking and timeout as the listener's,
    /// and its address where it was asked for.
    fn accept(
        &mut self,
        timeout: Option<Duration>,
        want_peer: bool,
    ) -> Result<(Socket, Option<Vec<u8>>), String> {
        let sock = self.sock.as_ref().ok_or_else(String::new)?;
        match php_heap::poll_fd(sock.fd(), Readiness::Readable, timeout) {
            Ok(true) => {}
            Ok(false) => {
                return Err(socket_error(&std::io::Error::from_raw_os_error(ETIMEDOUT)).1);
            }
            Err(error) => return Err(socket_error(&error).1),
        }
        let (client, peer) = match sock {
            Sock::TcpServer(listener) => {
                let (client, addr) = listener.accept().map_err(|e| socket_error(&e).1)?;
                (Sock::Tcp(client), inet_text(&addr))
            }
            Sock::UnixServer(listener) => {
                let (client, addr) = listener.accept().map_err(|e| socket_error(&e).1)?;
                (Sock::Unix(client), unix_text(&addr))
            }
            _ => {
                let error = std::io::Error::from_raw_os_error(95);
                return Err(socket_error(&error).1);
            }
        };
        let mut accepted = Socket::new(client, self.label, self.timeout);
        accepted.is_blocked = self.is_blocked;
        Ok((accepted, want_peer.then_some(peer)))
    }
}

/// `php_network_get_sock_name` and `php_network_get_peer_name`.
fn name(sock: &Sock, peer: bool) -> Option<Vec<u8>> {
    let inet = |addr: std::io::Result<SocketAddr>| addr.ok().map(|a| inet_text(&a));
    let unix =
        |addr: std::io::Result<std::os::unix::net::SocketAddr>| addr.ok().map(|a| unix_text(&a));
    match (sock, peer) {
        (Sock::Tcp(s), false) => inet(s.local_addr()),
        (Sock::Tcp(s), true) => inet(s.peer_addr()),
        (Sock::TcpServer(s), false) => inet(s.local_addr()),
        (Sock::Udp(s), false) => inet(s.local_addr()),
        (Sock::Udp(s), true) => inet(s.peer_addr()),
        (Sock::Unix(s), false) => unix(s.local_addr()),
        (Sock::Unix(s), true) => unix(s.peer_addr()),
        (Sock::UnixServer(s), false) => unix(s.local_addr()),
        (Sock::Udg(s), false) => unix(s.local_addr()),
        (Sock::Udg(s), true) => unix(s.peer_addr()),
        _ => None,
    }
}

/// `sock_recvfrom` (`Z/main/streams/xp_socket.c:305`): `recvfrom(2)`, or
/// `recv(2)` with `MSG_PEEK`, and the sender's address as text where it
/// was asked for, empty where the socket gives none.
fn recv(
    sock: &mut Sock,
    buf: &mut [u8],
    peek: bool,
    want_addr: bool,
) -> Option<(usize, Option<Vec<u8>>)> {
    if peek {
        if let Sock::Udp(s) = sock {
            let (n, from) = s.peek_from(buf).ok()?;
            return Some((n, want_addr.then(|| inet_text(&from))));
        }
        let n = php_heap::peek_fd(sock.fd(), buf).ok()?;
        return Some((n, want_addr.then(Vec::new)));
    }
    match sock {
        Sock::Udp(s) => {
            let (n, from) = s.recv_from(buf).ok()?;
            Some((n, want_addr.then(|| inet_text(&from))))
        }
        Sock::Udg(s) => {
            let (n, from) = s.recv_from(buf).ok()?;
            Some((n, want_addr.then(|| unix_text(&from))))
        }
        sock => {
            let n = sock.read(buf).ok()?;
            Some((n, want_addr.then(Vec::new)))
        }
    }
}

/// `parse_ip_address_ex` (`Z/main/streams/xp_socket.c:651`): the host
/// and the `port` of `host:port`, `[v6]:port` for IPv6, the `port` as
/// `atoi` reads it; the error text where there is no `port`.
fn parse_ip_address(name: &[u8]) -> Result<(String, u16), String> {
    let shown = String::from_utf8_lossy(name);
    if name.contains(&0) {
        return Err(String::from("The hostname must not contain null bytes"));
    }
    if name.first() == Some(&b'[') && name.len() > 1 {
        let inner = &name[1..name.len() - 1];
        let Some(close) = inner.iter().position(|b| *b == b']') else {
            return Err(format!("Failed to parse IPv6 address \"{shown}\""));
        };
        if name.get(close + 2) != Some(&b':') {
            return Err(format!("Failed to parse IPv6 address \"{shown}\""));
        }
        let host = String::from_utf8_lossy(&name[1..=close]).into_owned();
        return Ok((host, atoi_port(&name[close + 3..])));
    }
    let head = &name[..name.len().saturating_sub(1)];
    let Some(colon) = head.iter().position(|b| *b == b':') else {
        return Err(format!("Failed to parse address \"{shown}\""));
    };
    let host = String::from_utf8_lossy(&name[..colon]).into_owned();
    Ok((host, atoi_port(&name[colon + 1..])))
}

/// `atoi` of a `port`, cut to the `unsigned short` php hands `htons`.
fn atoi_port(text: &[u8]) -> u16 {
    let text = text.trim_ascii_start();
    let (negative, digits) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let mut value: i64 = 0;
    for digit in digits.iter().take_while(|b| b.is_ascii_digit()) {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
    }
    if negative {
        value = -value;
    }
    u16::try_from(value.rem_euclid(0x1_0000)).unwrap_or(0)
}

/// `php_network_getaddresses` (`Z/main/network.c:148`): the addresses
/// `getaddrinfo(3)` gives `host`, each with `port`; a lookup that fails
/// is php's warning, and its text is the error.
fn addresses(req: &mut Request, host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    match (host, port).to_socket_addrs() {
        Ok(addrs) => Ok(addrs.collect()),
        Err(error) => {
            let text = error.to_string();
            let gai = text
                .strip_prefix("failed to lookup address information: ")
                .unwrap_or(&text);
            let message = format!("php_network_getaddresses: getaddrinfo for {host} failed: {gai}");
            let shown = format!("{}(): {message}", req.active_native());
            warning(req, &shown);
            Err(message)
        }
    }
}

/// `sun_path`'s room less its terminating NUL, the longest path
/// `parse_unix_address` keeps.
const UNIX_PATH_MAX: usize = 103;

/// `parse_unix_address` (`Z/main/streams/xp_socket.c:608`): the path, cut
/// to what `sun_path` holds after php's notice.
fn unix_path(req: &mut Request, name: &[u8]) -> std::path::PathBuf {
    let mut path = name;
    if path.len() > UNIX_PATH_MAX {
        path = &path[..UNIX_PATH_MAX];
        let text = format!(
            "{}(): socket path exceeded the maximum allowed length of {UNIX_PATH_MAX} bytes and was truncated",
            req.active_native()
        );
        raise(req, Level::Notice, &text);
    }
    std::path::PathBuf::from(std::ffi::OsStr::from_bytes(path))
}

/// An error of `std`'s as the failure `php_stream_xport_create` reports.
fn failure(error: &std::io::Error) -> XportFailure {
    let (code, text) = socket_error(error);
    XportFailure { code, text }
}

/// `php_tcp_sockop_connect` (`Z/main/streams/xp_socket.c:763`) for
/// `tcp` and `udp`: each address the host resolves to tried in turn, a
/// TCP socket connected within `timeout` and a UDP one bound and given
/// its peer; the last failure is the error.
fn connect_inet(
    req: &mut Request,
    name: &[u8],
    udp: bool,
    timeout: Option<Duration>,
) -> Result<Sock, XportFailure> {
    let text_only = |text| XportFailure { code: 0, text };
    let (host, port) = parse_ip_address(name).map_err(text_only)?;
    let addrs = addresses(req, &host, port).map_err(text_only)?;
    let mut last = XportFailure {
        code: 0,
        text: String::new(),
    };
    for addr in addrs {
        let made = if udp {
            let local: SocketAddr = if addr.is_ipv4() {
                (std::net::Ipv4Addr::UNSPECIFIED, 0).into()
            } else {
                (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
            };
            UdpSocket::bind(local).and_then(|s| s.connect(addr).map(|()| Sock::Udp(s)))
        } else {
            match timeout {
                Some(t) => TcpStream::connect_timeout(&addr, t.max(Duration::from_micros(1))),
                None => TcpStream::connect(addr),
            }
            .map(Sock::Tcp)
        };
        match made {
            Ok(sock) => return Ok(sock),
            Err(error) => last = failure(&error),
        }
    }
    Err(last)
}

/// `php_tcp_sockop_bind` (`Z/main/streams/xp_socket.c:694`) for `tcp` and
/// `udp`: the first address the host resolves to that binds, with
/// `SO_REUSEADDR` as `std`'s listener sets it, and a TCP one listening.
fn bind_inet(req: &mut Request, name: &[u8], udp: bool) -> Result<Sock, XportFailure> {
    let text_only = |text| XportFailure { code: 0, text };
    let (host, port) = parse_ip_address(name).map_err(text_only)?;
    let addrs = addresses(req, &host, port).map_err(text_only)?;
    let made = if udp {
        UdpSocket::bind(addrs.as_slice()).map(Sock::Udp)
    } else {
        TcpListener::bind(addrs.as_slice()).map(Sock::TcpServer)
    };
    made.map_err(|error| failure(&error))
}

/// The `transports` `PHP_MINIT(standard)` registers
/// (`Z/ext/standard/basic_functions.c:315`), in its order, each with the
/// label of the ops `php_stream_generic_socket_factory` picks for it.
const TRANSPORTS: [(&str, &str); 4] = [
    ("tcp", "tcp_socket"),
    ("udp", "udp_socket"),
    ("unix", "unix_socket"),
    ("udg", "udg_socket"),
];

impl Request {
    /// `stream_get_transports()`: the registered `transports` in the order
    /// they were registered, `tcp`, `udp`, `unix` and `udg`.
    #[must_use]
    pub fn transports(&self) -> Vec<&'static str> {
        TRANSPORTS.iter().map(|(name, _)| *name).collect()
    }

    /// `_php_stream_xport_create` (`Z/main/streams/transports.c:52`) under
    /// `REPORT_ERRORS` with an error string asked for: the `transport` read
    /// off the front of `name`, `tcp` where it names none, one nobody
    /// registered being php's `Unable to find the socket transport`; then
    /// a client connected to the rest of the name, within `timeout`, or a
    /// server bound to it, and listening where `listen` says. The stream
    /// is `r+`, of the `transport`'s kind, with the name as written as its
    /// `uri` and `default_socket_timeout` as its read timeout. A failure
    /// is its error code and text, which the natives print; the `Err` is
    /// the memory limit.
    pub fn xport_create(
        &mut self,
        name: &[u8],
        xport: Xport,
    ) -> Result<Result<Stream, XportFailure>, Thrown> {
        let n = name
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let (protocol, rest) =
            if n > 1 && name.get(n) == Some(&b':') && name[n + 1..].starts_with(b"//") {
                (&name[..n], &name[n + 3..])
            } else {
                (&b"tcp"[..], name)
            };
        let Some(&(_, label)) = TRANSPORTS
            .iter()
            .find(|(registered, _)| registered.as_bytes() == protocol)
        else {
            let shown = String::from_utf8_lossy(&protocol[..protocol.len().min(31)]);
            return Ok(Err(XportFailure {
                code: 0,
                text: format!(
                    "Unable to find the socket transport \"{shown}\" - did you forget to enable it when you configured PHP?"
                ),
            }));
        };
        let made = match (label, xport) {
            ("tcp_socket", Xport::Client { timeout }) => connect_inet(self, rest, false, timeout),
            ("udp_socket", Xport::Client { timeout }) => connect_inet(self, rest, true, timeout),
            ("tcp_socket", Xport::Server { .. }) => bind_inet(self, rest, false),
            ("udp_socket", Xport::Server { .. }) => bind_inet(self, rest, true),
            ("unix_socket", Xport::Client { .. }) => {
                let path = unix_path(self, rest);
                UnixStream::connect(path)
                    .map(Sock::Unix)
                    .map_err(|e| failure(&e))
            }
            ("unix_socket", Xport::Server { .. }) => {
                let path = unix_path(self, rest);
                UnixListener::bind(path)
                    .map(Sock::UnixServer)
                    .map_err(|_| XportFailure {
                        code: 0,
                        text: String::new(),
                    })
            }
            ("udg_socket", Xport::Client { .. }) => {
                let path = unix_path(self, rest);
                UnixDatagram::unbound()
                    .and_then(|s| s.connect(path).map(|()| Sock::Udg(s)))
                    .map_err(|e| failure(&e))
            }
            (_, _) => {
                let path = unix_path(self, rest);
                UnixDatagram::bind(path)
                    .map(Sock::Udg)
                    .map_err(|_| XportFailure {
                        code: 0,
                        text: String::new(),
                    })
            }
        };
        let sock = match made {
            Ok(sock) => sock,
            Err(failed) => return Ok(Err(failed)),
        };
        let mut kind = Socket::new(sock, label, default_timeout(self));
        kind.no_io = matches!(xport, Xport::Server { .. });
        let mut stream = PhpStream::new(Box::new(kind), b"r+");
        stream.orig_path = Some(name.to_vec());
        stream.flags.no_seek = true;
        Ok(Ok(self.stream_alloc(stream)?))
    }

    /// `php_stream_sock_open_from_socket` (`Z/main/network.c:1020`): a
    /// socket the stream is handed, of the `generic_socket` kind, `r+`,
    /// blocking, with `default_socket_timeout` as its read timeout.
    pub(crate) fn socket_stream(&mut self, fd: OwnedFd) -> Result<Stream, Thrown> {
        let kind = Socket::new(
            Sock::Generic(UnixStream::from(fd)),
            "generic_socket",
            default_timeout(self),
        );
        let mut stream = PhpStream::new(Box::new(kind), b"r+");
        stream.flags.no_seek = true;
        self.stream_alloc(stream)
    }
}

/// `php_network_parse_network_address_with_port` (`Z/main/network.c:560`):
/// `ip:port` or `[v6]:port` of a written-out address, no lookup.
fn parse_network_address(text: &[u8]) -> Option<SocketAddr> {
    let text = std::str::from_utf8(text).ok()?;
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = text.rsplit_once(':')?;
    let port = atoi_port(port.as_bytes());
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some(SocketAddr::new(host.parse().ok()?, port))
}

/// What an accept came to: the client and, where it was asked for, its
/// address, or the error text, `None` for a stream that is no socket.
type Accepted = Result<(Stream, Option<PhpString>), Option<String>>;

impl Stream {
    /// `php_stream_xport_accept` (`Z/main/streams/transports.c:283`): the
    /// next client of a listening socket within `timeout`, `None` to wait
    /// for ever, as a stream of the listener's kind, `r+`, with the
    /// client's address where `want_peer` asks for it; the error text of
    /// an accept that failed or ran out of time, `None` for a stream
    /// that is no socket.
    pub fn accept(
        self,
        req: &mut Request,
        timeout: Option<Duration>,
        want_peer: bool,
    ) -> Result<Accepted, Thrown> {
        let answered = req
            .stream_op(self, |req, stream| {
                stream
                    .ops
                    .xport(req, XportOp::Accept { timeout, want_peer })
            })
            .flatten();
        let Some(Xported::Accepted(accepted)) = answered else {
            return Ok(Err(None));
        };
        let (client, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(text) => return Ok(Err(Some(text))),
        };
        let mut stream = PhpStream::new(Box::new(client), b"r+");
        stream.flags.no_seek = true;
        let client = req.stream_alloc(stream)?;
        let peer = match peer {
            Some(peer) => Some(PhpString::new(req, &peer)?),
            None => None,
        };
        Ok(Ok((client, peer)))
    }

    /// `php_stream_xport_get_name` (`Z/main/streams/transports.c:327`):
    /// the socket's own address or, with `peer`, its peer's, as
    /// `php_network_populate_name_from_sockaddr` writes it; `None` where
    /// the stream is no socket, the call failed or the name is empty.
    pub fn socket_name(self, req: &mut Request, peer: bool) -> Result<Option<PhpString>, Thrown> {
        let answered = req
            .stream_op(self, |req, stream| {
                stream.ops.xport(req, XportOp::Name { peer })
            })
            .flatten();
        match answered {
            Some(Xported::Name(Some(name))) if name.first().is_some_and(|b| *b != 0) => {
                Ok(Some(PhpString::new(req, &name)?))
            }
            _ => Ok(None),
        }
    }

    /// `php_stream_xport_sendto` (`Z/main/streams/transports.c:462`)
    /// after `stream_socket_sendto` parsed the address: `to` is `ip:port`
    /// or `[v6]:port`, and `None` back is php's warning that it is
    /// neither. Then the socket's `send`, to `to` where it is given,
    /// whatever the bits, since php's shortcut to `php_stream_write` is
    /// compiled out; `STREAM_OOB` is taken and not honoured. The count,
    /// or php's `-1`, which a stream that is no socket answers too.
    pub fn sendto(
        self,
        req: &mut Request,
        data: &[u8],
        flags: i64,
        to: Option<&[u8]>,
    ) -> Option<i64> {
        let addr = match to {
            Some(text) => {
                let Some(addr) = parse_network_address(text) else {
                    let shown = format!(
                        "{}(): Failed to parse `{}' into a valid network address",
                        req.active_native(),
                        String::from_utf8_lossy(text)
                    );
                    warning(req, &shown);
                    return None;
                };
                Some(addr)
            }
            None => None,
        };
        // `MSG_OOB` has no spelling in `std`'s sockets; the bit is read
        // and goes no further.
        let _ = flags & STREAM_OOB;
        let answered = req
            .stream_op(self, |req, stream| {
                stream.ops.xport(
                    req,
                    XportOp::Send {
                        buf: data,
                        to: addr,
                    },
                )
            })
            .flatten();
        Some(match answered {
            Some(Xported::Sent(Some(n))) => i64::try_from(n).unwrap_or(i64::MAX),
            _ => -1,
        })
    }

    /// `php_stream_xport_recvfrom` (`Z/main/streams/transports.c:398`):
    /// the socket's `recv` straight, past the read buffer, whatever the
    /// bits, since php's read through the buffer is compiled out; under
    /// `STREAM_PEEK` the bytes are left queued, and the sender's address
    /// comes back where `want_addr` asks for it, empty where the socket
    /// gives none. `None` is php's `-1`, which a stream that is no socket
    /// answers too.
    pub fn recvfrom(
        self,
        req: &mut Request,
        len: usize,
        flags: i64,
        want_addr: bool,
    ) -> Result<Option<(PhpString, Option<PhpString>)>, Thrown> {
        let peek = flags & STREAM_PEEK != 0;
        let mut buf = vec![0u8; len];
        let answered = req
            .stream_op(self, |req, stream| {
                stream.ops.xport(
                    req,
                    XportOp::Recv {
                        buf: &mut buf,
                        peek,
                        want_addr,
                    },
                )
            })
            .flatten();
        let Some(Xported::Received(Some((n, from)))) = answered else {
            return Ok(None);
        };
        let bytes = PhpString::new(req, &buf[..n])?;
        let from = match from {
            Some(from) => Some(PhpString::new(req, &from)?),
            None => None,
        };
        Ok(Some((bytes, from)))
    }

    /// `php_stream_xport_shutdown` (`Z/main/streams/transports.c:476`):
    /// `shutdown(2)` of one direction or both; whether it answered `0`.
    pub fn shutdown(self, req: &mut Request, how: Shutdown) -> bool {
        matches!(
            req.stream_op(self, |req, stream| stream
                .ops
                .xport(req, XportOp::Shutdown(how)))
                .flatten(),
            Some(Xported::Shut(true))
        )
    }
}

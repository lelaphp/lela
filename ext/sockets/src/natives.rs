//! The natives of `sockets.c` over one socket's fd, in
//! `sockets.stub.php`'s order: making, binding, connecting, listening
//! and accepting, reading and writing, the names, the pair, shutting
//! down, and the errors.

use std::net::{SocketAddrV4, SocketAddrV6};
use std::os::fd::RawFd;

use php_ext::abi::{Instance, OrFalse, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::{c_str, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, argument_error, docref, throw_error,
};
use php_ext::request::{OptionAnswer, Request, StreamOption};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk};

use crate::State;
use crate::addr::{inet_addr, inet6_addr, ntop};
use crate::constants::PHP_NORMAL_READ;
use crate::ffi::{self, Addr, SUN_PATH};
use crate::socket::{
    Sock, Socket, Waited, global_error, is_transient, live_stream, made, new_socket, release, sock,
    socket_error, strerror, valid, wait_on, wait_on_one,
};

/// A by-reference parameter with no declared type, `&$address`.
pub(crate) type Out = Ref<Untyped<Value>>;

/// A C `int` handed a `zend_long`: its low 32 bits, as C converts it.
#[allow(clippy::cast_possible_truncation)]
pub(crate) const fn c_int(n: i64) -> i32 {
    n as i32
}

/// `(unsigned short)port`: the low 16 bits.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn ushort(n: i64) -> u16 {
    n as u16
}

/// A count of bytes as a `zend_long`.
pub(crate) fn long(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A string of the request's holding `bytes`.
pub(crate) fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, bytes)?.into_value())
}

/// `zend_string_alloc(len)` before a read into it: the request's
/// memory limit met as php meets it, then the room.
pub(crate) fn room(req: &mut Request, len: usize) -> Result<Vec<u8>, Thrown> {
    let asked = PhpString::filled(req, len, |_| {})?;
    asked.give_up(req);
    Ok(vec![0; len])
}

/// Assign through a by-reference parameter.
pub(crate) fn assign(
    req: &mut Request,
    out: &Out,
    value: impl php_ext::abi::IntoReturn,
) -> Result<(), Thrown> {
    out.set(req, Untyped(value))
}

/// A socket laid down and not yet opened, `object_init_ex` before the
/// call that opens it, so its object id is taken first as php takes it.
fn laid(req: &mut Request) -> Result<Owner<PhpObject>, Thrown> {
    made(req, b"Socket")
}

/// The socket `laid` gave, opened over `sock`.
fn opened(
    req: &mut Request,
    object: Owner<PhpObject>,
    sock: Sock,
) -> OrFalse<Owner<Instance<Socket>>> {
    if let Some(payload) = object.native_mut::<Sock>(req) {
        *payload = sock;
    }
    OrFalse::Value(object.into_instance())
}

/// `zval_ptr_dtor(return_value); RETURN_FALSE`.
fn dropped<T>(req: &mut Request, object: Owner<PhpObject>) -> OrFalse<T> {
    object.give_up(req);
    OrFalse::False
}

/// The family a socket was made for.
fn family(req: &mut Request, socket: Instance<Socket>) -> Result<i32, Thrown> {
    Ok(sock(req, socket)?.family)
}

/// `php_sock_array_to_fd_set` (`Z/ext/sockets/sockets.c:559`): one
/// array's sockets with their fds, php's `TypeError` for an element
/// that is none or is closed; `None` for no array.
fn to_fd_set(
    req: &mut Request,
    position: u32,
    array: Option<PhpArray>,
) -> Result<Option<Vec<(PhpObject, RawFd)>>, Thrown> {
    let Some(array) = array else {
        return Ok(None);
    };
    let mut fds = Vec::new();
    let mut refused = None;
    for (_, element) in walk::rows(req, array) {
        let element = element.deref();
        let (object, fd) = match element.as_ref() {
            ValueRef::Object(object) if object.native::<Sock>(req).is_some() => {
                (object, object.native::<Sock>(req).and_then(Sock::fd))
            }
            _ => {
                refused = Some(Refused::Given(value_name(req, &element)));
                break;
            }
        };
        let Some(fd) = fd else {
            refused = Some(Refused::Closed);
            break;
        };
        fds.push((object, fd));
    }
    match refused {
        None => Ok(Some(fds)),
        Some(Refused::Given(given)) => {
            let tail = format!("only have elements of type Socket, {given} given");
            Err(arg_type_error(req, position, &tail))
        }
        Some(Refused::Closed) => Err(closed_in_set(req, position)),
    }
}

/// `php_sock_array_to_fd_set`'s `TypeError` for a closed socket in the
/// array at argument `position`.
fn closed_in_set(req: &mut Request, position: u32) -> Thrown {
    argument_error(req, "TypeError", position, "contains a closed socket")
}

/// Why [`to_fd_set`] refused an element, read in the walk and raised
/// after it.
enum Refused {
    /// An element that is no socket, by its type's name.
    Given(String),
    /// A socket closed already.
    Closed,
}

/// `php_sock_array_from_fd_set` (`Z/ext/sockets/sockets.c:592`): a new
/// array of the elements `select(2)` left set, each under its key.
fn from_fd_set(
    req: &mut Request,
    target: &Ref<Option<PhpArray>>,
    array: PhpArray,
    ready: &[bool],
) -> Result<(), Thrown> {
    let mut out = PhpArray::new(req)?;
    // `php_sock_array_from_fd_set`'s `ZEND_HASH_FOREACH_KEY_VAL`, which no
    // code of the program reaches: the rows read under the borrowed
    // request, views the argument keeps, then written.
    let rows: Vec<(Key, Value)> = walk::rows(req, array)
        .map(|(key, element)| (key, element.deref()))
        .collect();
    for ((key, element), set) in rows.into_iter().zip(ready) {
        if *set {
            out.set(req, Owner::take(req, key), Owner::take(req, element))?;
        }
    }
    target.set(req, Some(out))
}

/// `socket_select(?array &$read, ?array &$write, ?array &$except, ?int
/// $seconds, int $microseconds = 0): int|false`: the sockets of the
/// three arrays handed to `select(2)` and each array cut to those it
/// left ready.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// socket_write($p[0], "x");
/// $r = ['a' => $p[1], 'b' => $p[0]]; $w = [$p[0]]; $e = null;
/// var_dump(socket_select($r, $w, $e, 0), array_keys($r), count($w), $e);
/// $r = $w = $e = null;
/// try { socket_select($r, $w, $e, 0); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
/// $r = [1];
/// try { socket_select($r, $w, $e, 0); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
/// socket_close($p[0]); $r = [$p[0]];
/// try { socket_select($r, $w, $e, 0); } catch (TypeError $x) { echo $x->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// array(1) {
///   [0]=>
///   string(1) "a"
/// }
/// int(1)
/// NULL
/// socket_select(): At least one array argument must be passed
/// socket_select(): Argument #1 ($read) must only have elements of type Socket, int given
/// socket_select(): Argument #1 ($read) contains a closed socket
/// ```
#[php_function]
fn socket_select(
    req: &mut Request,
    read: Ref<Option<PhpArray>>,
    write: Ref<Option<PhpArray>>,
    except: Ref<Option<PhpArray>>,
    seconds: Option<i64>,
    #[php(default = 0)] microseconds: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let arrays = [&read, &write, &except].map(|each| each.get().copied().flatten());
    let mut sets: [Option<Vec<(PhpObject, RawFd)>>; 3] = [None, None, None];
    for (at, (set, array)) in sets.iter_mut().zip(arrays).enumerate() {
        *set = to_fd_set(req, u32::try_from(at).unwrap_or(0) + 1, array)?;
    }
    if sets.iter().flatten().all(Vec::is_empty) {
        return Err(throw_error(
            req,
            "ValueError",
            "socket_select(): At least one array argument must be passed",
        ));
    }
    let max_fd = sets
        .iter()
        .flatten()
        .flatten()
        .map(|(_, fd)| *fd)
        .max()
        .unwrap_or(0);
    if max_fd >= ffi::FD_SETSIZE {
        let text = format!(
            "You MUST recompile PHP with a larger value of FD_SETSIZE.\nIt is set to {}, but you have descriptors numbered at least as high as {max_fd}.\n --enable-fd-setsize={} is recommended, but you may want to set it\nto equal the maximum number of open files supported by your system,\nin order to avoid seeing this error again at a later date.",
            ffi::FD_SETSIZE,
            (max_fd + 1024) & !1023
        );
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    }
    let timeout = seconds.map(|sec| {
        if microseconds > 999_999 {
            (sec + microseconds / 1_000_000, microseconds % 1_000_000)
        } else {
            (sec, microseconds)
        }
    });
    // The sockets of the three arrays in one list for the wait, which
    // holds each out of reach of a callback served in it (#2601), and the
    // arrays held for the walk after it, whatever the callback assigns to
    // the variables.
    let [r_len, w_len, _] = sets.each_ref().map(|set| set.as_ref().map_or(0, Vec::len));
    let (sockets, positions): (Vec<PhpObject>, Vec<u32>) = (1..)
        .zip(&sets)
        .flat_map(|(position, set)| {
            set.iter()
                .flatten()
                .map(move |(object, _)| (*object, position))
        })
        .unzip();
    let held: Vec<Owner<PhpArray>> = arrays
        .iter()
        .flatten()
        .map(|array| Owner::take(req, *array))
        .collect();
    let waited = wait_on(req, &sockets, |fds| {
        let (r, rest) = fds.split_at(r_len.min(fds.len()));
        let (w, e) = rest.split_at(w_len.min(rest.len()));
        ffi::select([r, w, e], timeout)
    });
    let selected = match waited {
        Waited::Answered(selected) => selected,
        Waited::Closed { at, .. } => {
            for array in held {
                array.give_up(req);
            }
            let position = positions.get(at).copied().unwrap_or(1);
            return Err(closed_in_set(req, position));
        }
    };
    let answered = select_answered(req, [&read, &write, &except], arrays, selected);
    for array in held {
        array.give_up(req);
    }
    answered
}

/// What `socket_select` answers once its wait returned: each array cut
/// to the sockets `select(2)` left ready and their count, or php's
/// warning and `false` for a failed call.
fn select_answered(
    req: &mut Request,
    targets: [&Ref<Option<PhpArray>>; 3],
    arrays: [Option<PhpArray>; 3],
    selected: Result<(i32, [Vec<bool>; 3]), nix::errno::Errno>,
) -> Result<OrFalse<i64>, Thrown> {
    let (count, ready) = match selected {
        Ok(answered) => answered,
        Err(errno) => {
            let code = errno as i32;
            req.state::<State>().last_error = code;
            let text = format!("Unable to select [{code}]: {}", strerror(req, code));
            docref(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    };
    for ((target, array), ready) in targets.into_iter().zip(arrays).zip(&ready) {
        if let Some(array) = array {
            from_fd_set(req, target, array, ready)?;
        }
    }
    Ok(OrFalse::Value(i64::from(count)))
}

/// `socket_create_listen(int $port, int $backlog = SOMAXCONN):
/// Socket|false`, `php_open_listen_sock`: a TCP socket bound to
/// `0.0.0.0` at the `port` and listening.
///
/// ```php
/// $s = socket_create_listen(0);
/// socket_getsockname($s, $a, $p);
/// $c = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump($a, socket_connect($c, '127.0.0.1', $p));
/// try { socket_create_listen(65536); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(7) "0.0.0.0"
/// bool(true)
/// socket_create_listen(): Argument #1 ($port) must be between 0 and 65535
/// ```
#[php_function]
fn socket_create_listen(
    req: &mut Request,
    port: i64,
    #[php(default = SOMAXCONN)] backlog: i64,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    if !(0..=65535).contains(&port) {
        return Err(arg_value_error(req, 1, "be between 0 and 65535"));
    }
    let owner = laid(req)?;
    let object = *owner;
    let fd = match ffi::socket(libc::AF_INET, libc::SOCK_STREAM, 0) {
        Ok(fd) => fd,
        Err(errno) => {
            socket_error(
                req,
                object,
                "unable to create listening socket",
                errno as i32,
            );
            return Ok(dropped(req, owner));
        }
    };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    let any = Addr::V4(SocketAddrV4::new(
        std::net::Ipv4Addr::UNSPECIFIED,
        ushort(port),
    ));
    if let Err(errno) = ffi::bind(raw, &any) {
        socket_error(req, object, "unable to bind to given address", errno as i32);
        return Ok(dropped(req, owner));
    }
    if let Err(errno) = ffi::listen(raw, c_int(backlog)) {
        socket_error(req, object, "unable to listen on socket", errno as i32);
        return Ok(dropped(req, owner));
    }
    Ok(opened(req, owner, Sock::owning(fd, libc::AF_INET)))
}

/// `socket_accept(Socket $socket): Socket|false`,
/// `php_accept_connect`: the next connection, as a socket of the
/// family its address has, blocking.
///
/// ```php
/// $l = socket_create_listen(0); socket_getsockname($l, $a, $port);
/// $c = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump(socket_connect($c, '127.0.0.1', $port));
/// $a = socket_accept($l);
/// var_dump($a instanceof Socket, socket_write($c, "hi"), socket_read($a, 10));
/// socket_set_nonblock($l);
/// var_dump(socket_accept($l), socket_last_error() === SOCKET_EAGAIN);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(2)
/// string(2) "hi"
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn socket_accept(
    req: &mut Request,
    socket: Instance<Socket>,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    valid(req, socket)?;
    let blocking = sock(req, socket)?.blocking;
    let owner = laid(req)?;
    let object = *owner;
    let accepted = match wait_on_one(req, socket, |fd| accept(fd, blocking)) {
        Ok(accepted) => accepted,
        Err(thrown) => {
            owner.give_up(req);
            return Err(thrown);
        }
    };
    let (new, family) = match accepted {
        Ok(accepted) => accepted,
        Err(errno) => {
            socket_error(
                req,
                object,
                "unable to accept incoming connection",
                errno as i32,
            );
            return Ok(dropped(req, owner));
        }
    };
    #[cfg(not(target_os = "linux"))]
    if let Err(errno) = ffi::set_cloexec(std::os::fd::AsRawFd::as_raw_fd(&new)) {
        socket_error(
            req,
            object,
            "unable to set cloexec mode on the socket",
            errno as i32,
        );
        return Ok(dropped(req, owner));
    }
    Ok(opened(req, owner, Sock::owning(new, family)))
}

/// `php_accept_connect`'s call: `accept4(2)` where php has
/// `HAVE_ACCEPT4`, the new fd close-on-exec and, under a listener that
/// is not blocking, non-blocking as it is made; `accept(2)` elsewhere,
/// with close-on-exec set after it.
#[cfg(target_os = "linux")]
fn accept(fd: RawFd, blocking: bool) -> Result<(std::os::fd::OwnedFd, i32), nix::errno::Errno> {
    let flags = if blocking {
        libc::SOCK_CLOEXEC
    } else {
        libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK
    };
    ffi::accept4(fd, flags)
}

/// [`accept`] where there is no `accept4(2)`.
#[cfg(not(target_os = "linux"))]
fn accept(fd: RawFd, _blocking: bool) -> Result<(std::os::fd::OwnedFd, i32), nix::errno::Errno> {
    ffi::accept(fd)
}

/// `socket_set_nonblock` and `socket_set_block`: the stream's own
/// option where the socket came from one, and `php_set_sock_blocking`
/// otherwise.
fn set_blocking(req: &mut Request, socket: Instance<Socket>, block: bool) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    if let Some(stream) = sock(req, socket)?
        .stream()
        .and_then(|s| live_stream(req, s))
        && stream.set_option(req, StreamOption::Blocking(block)) != OptionAnswer::Err
    {
        sock(req, socket)?.blocking = block;
        return Ok(true);
    }
    match ffi::set_blocking(fd, block) {
        Ok(()) => {
            sock(req, socket)?.blocking = block;
            Ok(true)
        }
        Err(errno) => {
            let msg = if block {
                "unable to set blocking mode"
            } else {
                "unable to set nonblocking mode"
            };
            socket_error(req, socket.object(), msg, errno as i32);
            Ok(false)
        }
    }
}

/// `socket_set_nonblock(Socket $socket): bool`.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// var_dump(socket_set_nonblock($p[0]), socket_read($p[0], 10), socket_last_error($p[0]) === SOCKET_EAGAIN);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn socket_set_nonblock(req: &mut Request, socket: Instance<Socket>) -> Result<bool, Thrown> {
    set_blocking(req, socket, false)
}

/// `socket_set_block(Socket $socket): bool`.
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $k = socket_import_stream($s);
/// var_dump(socket_set_nonblock($k), stream_get_meta_data($s)['blocked'], socket_set_block($k), stream_get_meta_data($s)['blocked']);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn socket_set_block(req: &mut Request, socket: Instance<Socket>) -> Result<bool, Thrown> {
    set_blocking(req, socket, true)
}

/// `socket_listen(Socket $socket, int $backlog = 0): bool`.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump(socket_bind($s, '127.0.0.1'), socket_listen($s, 1));
/// $u = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
/// var_dump(socket_listen($u));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: socket_listen(): unable to listen on socket [102]: Operation not supported on socket in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn socket_listen(
    req: &mut Request,
    socket: Instance<Socket>,
    #[php(default = 0)] backlog: i64,
) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    if let Err(errno) = ffi::listen(fd, c_int(backlog)) {
        socket_error(
            req,
            socket.object(),
            "unable to listen on socket",
            errno as i32,
        );
        return Ok(false);
    }
    Ok(true)
}

/// `socket_close(Socket $socket): void`: the stream freed where the
/// socket came from one or went to one, the fd closed
/// otherwise; the socket is closed after either.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// socket_close($s);
/// try { socket_close($s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $t = stream_socket_server('tcp://127.0.0.1:0');
/// socket_close(socket_import_stream($t));
/// try { fclose($t); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// socket_close(): Argument #1 ($socket) has already been closed
/// fclose(): Argument #1 ($stream) must be an open stream resource
/// ```
#[php_function]
fn socket_close(req: &mut Request, socket: Instance<Socket>) -> Result<(), Thrown> {
    valid(req, socket)?;
    if let Some(handle) = sock(req, socket)?.close() {
        release(req, handle);
    }
    Ok(())
}

/// `socket_write(Socket $socket, string $data, ?int $length = null):
/// int|false`: one `write(2)` of the data, or of its first `$length`
/// bytes.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// var_dump(socket_write($p[0], "hello", 3), socket_read($p[1], 10));
/// try { socket_write($p[0], "x", -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// socket_close($p[1]);
/// // The peer's close is seen before the write: a read of $p[0] returns
/// // its EOF once the close has landed, so the write below meets a closed
/// // peer on every run, whatever the load.
/// var_dump(socket_read($p[0], 1));
/// var_dump(@socket_write($p[0], "x"), socket_last_error($p[0]) === SOCKET_EPIPE);
/// ```
/// ```expect
/// int(3)
/// string(3) "hel"
/// socket_write(): Argument #3 ($length) must be greater than or equal to 0
/// string(0) ""
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn socket_write(
    req: &mut Request,
    socket: Instance<Socket>,
    data: PhpString,
    #[php(default = null)] length: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let fd = valid(req, socket)?;
    if length.is_some_and(|n| n < 0) {
        return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
    }
    let bytes = data.as_bytes();
    let n = length.map_or(bytes.len(), |n| {
        usize::try_from(n).unwrap_or(usize::MAX).min(bytes.len())
    });
    match ffi::write(fd, &bytes[..n]) {
        Ok(written) => Ok(OrFalse::Value(long(written))),
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "unable to write to socket",
                errno as i32,
            );
            Ok(OrFalse::False)
        }
    }
}

/// `php_read` (`Z/ext/sockets/sockets.c:352`): one byte at a time up
/// to a `\r` or a `\n`, which is kept, or `maxlen`; a socket that does
/// not block answers what it had on its second empty read.
fn read_line(fd: RawFd, buf: &mut [u8]) -> Result<usize, nix::errno::Errno> {
    use nix::errno::Errno;
    let nonblock = ffi::is_nonblocking(ffi::status_flags(fd)?);
    let maxlen = buf.len();
    let (mut n, mut m, mut no_read) = (0usize, 0isize, 0);
    let mut t = 0usize;
    buf[0] = 0;
    loop {
        if buf.get(t).is_some_and(|b| *b == b'\n' || *b == b'\r') || n >= maxlen {
            break;
        }
        if m > 0 {
            t += 1;
            n += 1;
        } else if m == 0 {
            no_read += 1;
            if nonblock && no_read >= 2 {
                return Ok(n);
            }
            if no_read > 200 {
                return Err(Errno::ECONNRESET);
            }
        }
        let mut errno = Errno::UnknownErrno;
        if n < maxlen {
            m = match ffi::recv(fd, &mut buf[t..=t], 0) {
                Ok(got) => isize::try_from(got).unwrap_or(0),
                Err(e) => {
                    errno = e;
                    -1
                }
            };
        }
        if errno != Errno::UnknownErrno && errno != Errno::ESPIPE && errno != Errno::EAGAIN {
            return Err(errno);
        }
    }
    if n < maxlen {
        n += 1;
    }
    Ok(n)
}

/// `socket_read(Socket $socket, int $length, int $mode =
/// PHP_BINARY_READ): string|false`: one `recv(2)`, or with
/// `PHP_NORMAL_READ` a line; `false` with no warning where a socket
/// that does not block has nothing.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// socket_write($p[0], "ab\ncd");
/// var_dump(socket_read($p[1], 10, PHP_NORMAL_READ), socket_read($p[1], 10), socket_read($p[1], 0));
/// socket_close($p[0]);
/// var_dump(socket_read($p[1], 10));
/// ```
/// ```expect
/// string(3) "ab
/// "
/// string(2) "cd"
/// bool(false)
/// string(0) ""
/// ```
#[php_function]
fn socket_read(
    req: &mut Request,
    socket: Instance<Socket>,
    length: i64,
    #[php(default = PHP_BINARY_READ)] mode: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    valid(req, socket)?;
    if length <= 0 || length == i64::MAX {
        return Ok(OrFalse::False);
    }
    let mut buf = room(req, usize::try_from(length).unwrap_or(usize::MAX))?;
    let read = wait_on_one(req, socket, |fd| {
        if mode == PHP_NORMAL_READ {
            read_line(fd, &mut buf)
        } else {
            ffi::recv(fd, &mut buf, 0)
        }
    })?;
    match read {
        Ok(n) => Ok(OrFalse::Value(PhpString::new(
            req,
            &buf[..n.min(buf.len())],
        )?)),
        Err(errno) => {
            let code = errno as i32;
            if is_transient(code) {
                sock(req, socket)?.error = code;
                req.state::<State>().last_error = code;
            } else {
                socket_error(req, socket.object(), "unable to read from socket", code);
            }
            Ok(OrFalse::False)
        }
    }
}

/// `socket_getsockname` and `socket_getpeername`: the address of an
/// inet socket and its `port`, or the path of a unix one.
fn name(
    req: &mut Request,
    socket: Instance<Socket>,
    address: &Out,
    port: &Out,
    peer: bool,
) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    let found = match ffi::name(fd, peer) {
        Ok(found) => found,
        Err(errno) => {
            let msg = if peer {
                "unable to retrieve peer name"
            } else {
                "unable to retrieve socket name"
            };
            socket_error(req, socket.object(), msg, errno as i32);
            return Ok(false);
        }
    };
    let text = ntop(&found);
    let at = match found {
        Addr::V4(v4) => Some(v4.port()),
        Addr::V6(v6) => Some(v6.port()),
        Addr::Unix(_) => None,
        #[cfg(target_os = "linux")]
        Addr::Other(libc::AF_PACKET) if !peer => {
            return packet_name(req, socket, fd, address, port);
        }
        Addr::Other(_) => {
            let tail = if peer {
                "be one of AF_UNIX, AF_INET, or AF_INET6"
            } else {
                "be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6"
            };
            return Err(arg_value_error(req, 1, tail));
        }
    };
    let value = string(req, &text)?;
    assign(req, address, value)?;
    if let Some(at) = at {
        assign(req, port, Value::int(i64::from(at)))?;
    }
    Ok(true)
}

/// `socket_getsockname`'s `AF_PACKET` arm: the name of the interface
/// the socket is bound to, and its index in `$port`; php's `Error`
/// where the index names no interface.
#[cfg(target_os = "linux")]
fn packet_name(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: RawFd,
    address: &Out,
    port: &Out,
) -> Result<bool, Thrown> {
    let index = match ffi::packet_index(fd) {
        Ok(index) => index,
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "unable to retrieve socket name",
                errno as i32,
            );
            return Ok(false);
        }
    };
    let named = u32::try_from(index)
        .ok()
        .and_then(|i| nix::net::if_::if_indextoname(i).ok());
    let Some(name) = named else {
        return Err(throw_error(req, "Error", "invalid interface index"));
    };
    let value = string(req, name.as_bytes())?;
    assign(req, address, value)?;
    assign(req, port, Value::int(i64::from(index)))?;
    Ok(true)
}

/// `socket_getsockname(Socket $socket, &$address, &$port = null): bool`.
///
/// ```php
/// $s = socket_create(AF_INET6, SOCK_DGRAM, SOL_UDP);
/// socket_bind($s, '::1');
/// var_dump(socket_getsockname($s, $a, $p), $a, $p > 0);
/// $u = socket_create(AF_UNIX, SOCK_STREAM, 0);
/// var_dump(socket_getsockname($u, $a, $p2), $a, isset($p2));
/// ```
/// ```expect
/// bool(true)
/// string(3) "::1"
/// bool(true)
/// bool(true)
/// string(0) ""
/// bool(false)
/// ```
#[php_function]
fn socket_getsockname(
    req: &mut Request,
    socket: Instance<Socket>,
    address: Out,
    #[php(default = null)] port: Out,
) -> Result<bool, Thrown> {
    name(req, socket, &address, &port, false)
}

/// `socket_getpeername(Socket $socket, &$address, &$port = null): bool`.
///
/// ```php
/// $l = socket_create_listen(0); socket_getsockname($l, $a, $port);
/// $c = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// var_dump(socket_getpeername($c, $a));
/// socket_connect($c, '127.0.0.1', $port);
/// var_dump(socket_getpeername($c, $a, $p), $a, $p === $port);
/// ```
/// ```expect
///
/// Warning: socket_getpeername(): unable to retrieve peer name [57]: Socket is not connected in %s on line 4
/// bool(false)
/// bool(true)
/// string(9) "127.0.0.1"
/// bool(true)
/// ```
#[php_function]
fn socket_getpeername(
    req: &mut Request,
    socket: Instance<Socket>,
    address: Out,
    #[php(default = null)] port: Out,
) -> Result<bool, Thrown> {
    name(req, socket, &address, &port, true)
}

/// The `$domain` and `$type` checks `socket_create` and
/// `socket_create_pair` share.
fn check_type(req: &mut Request, ty: i64) -> Result<(), Thrown> {
    #[cfg(not(target_os = "macos"))]
    let ty = ty & !i64::from(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK);
    if ty > 10 {
        #[cfg(target_os = "macos")]
        let tail = "be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM";
        #[cfg(not(target_os = "macos"))]
        let tail = "be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM optionally OR'ed with SOCK_CLOEXEC, SOCK_NONBLOCK";
        return Err(arg_value_error(req, 2, tail));
    }
    Ok(())
}

/// Whether `domain` is `AF_PACKET`, where the platform has it.
#[cfg(target_os = "linux")]
fn is_packet(domain: i64) -> bool {
    domain == i64::from(libc::AF_PACKET)
}

/// [`is_packet`] where there is no `AF_PACKET`.
#[cfg(not(target_os = "linux"))]
const fn is_packet(_: i64) -> bool {
    false
}

/// `PHP_ETH_PROTO_CHECK`: an `AF_PACKET` socket's protocol, an Ethernet
/// type no greater than `USHRT_MAX`, in network byte order, as `htons`
/// takes it, its low 16 bits; any other protocol as it is.
pub(crate) fn eth_proto(protocol: i64, family: i64) -> i64 {
    if is_packet(family) && protocol <= 0xffff {
        let low = u16::try_from(protocol.rem_euclid(0x1_0000)).unwrap_or(0);
        return i64::from(low.to_be());
    }
    protocol
}

fn inet_family(domain: i64) -> bool {
    [libc::AF_UNIX, libc::AF_INET, libc::AF_INET6]
        .iter()
        .any(|f| i64::from(*f) == domain)
}

/// `socket_create(int $domain, int $type, int $protocol): Socket|false`.
///
/// ```php
/// var_dump(socket_create(AF_INET, SOCK_STREAM, SOL_TCP) instanceof Socket);
/// try { socket_create(99, SOCK_STREAM, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_create(AF_INET, 11, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_create(AF_INET, SOCK_STREAM, 99999), socket_last_error() === SOCKET_EPROTONOSUPPORT);
/// ```
/// ```expect
/// bool(true)
/// socket_create(): Argument #1 ($domain) must be one of AF_UNIX, AF_PACKET, AF_INET6, or AF_INET
/// socket_create(): Argument #2 ($type) must be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM
///
/// Warning: socket_create(): Unable to create socket [43]: Protocol not supported in %s on line 5
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn socket_create(
    req: &mut Request,
    domain: i64,
    r#type: i64,
    protocol: i64,
) -> Result<OrFalse<Owner<Instance<Socket>>>, Thrown> {
    if !inet_family(domain) && !is_packet(domain) {
        return Err(arg_value_error(
            req,
            1,
            "be one of AF_UNIX, AF_PACKET, AF_INET6, or AF_INET",
        ));
    }
    check_type(req, r#type)?;
    let protocol = eth_proto(protocol, domain);
    let owner = laid(req)?;
    match ffi::socket(c_int(domain), c_int(r#type), c_int(protocol)) {
        Ok(fd) => Ok(opened(req, owner, Sock::owning(fd, c_int(domain)))),
        Err(errno) => {
            global_error(req, "Unable to create socket", errno as i32);
            Ok(dropped(req, owner))
        }
    }
}

/// The address a `socket_connect`, `socket_bind` or `socket_sendto`
/// is aimed at in the socket's family, or why there is none: a lookup
/// that warned, or php's `ValueError`.
fn target(
    req: &mut Request,
    socket: Instance<Socket>,
    address: &[u8],
    port: Option<i64>,
    port_at: u32,
    unix_at: u32,
    what: &str,
) -> Result<Option<Addr>, Thrown> {
    let object = socket.object();
    match family(req, socket)? {
        libc::AF_INET6 => {
            let Some(port) = port else {
                return Err(argument_error(
                    req,
                    "ValueError",
                    port_at,
                    "cannot be null when the socket type is AF_INET6",
                ));
            };
            Ok(inet6_addr(req, object, address)
                .map(|(ip, scope)| Addr::V6(SocketAddrV6::new(ip, ushort(port), 0, scope))))
        }
        libc::AF_INET => {
            let Some(port) = port else {
                return Err(argument_error(
                    req,
                    "ValueError",
                    port_at,
                    "cannot be null when the socket type is AF_INET",
                ));
            };
            Ok(inet_addr(req, object, address)
                .map(|ip| Addr::V4(SocketAddrV4::new(ip, ushort(port)))))
        }
        libc::AF_UNIX => {
            if address.len() >= SUN_PATH {
                let tail = format!("be less than {SUN_PATH}");
                return Err(arg_value_error(req, unix_at, &tail));
            }
            Ok(Some(Addr::Unix(address.to_vec())))
        }
        _ => Err(arg_value_error(req, 1, what)),
    }
}

/// `socket_connect(Socket $socket, string $address, ?int $port = null):
/// bool`.
///
/// ```php
/// $l = socket_create_listen(0); socket_getsockname($l, $a, $port);
/// $c = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// try { socket_connect($c, '127.0.0.1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_connect($c, 'localhost', $port));
/// $u = socket_create(AF_UNIX, SOCK_STREAM, 0);
/// var_dump(socket_connect($u, '/nonexistent/sock'));
/// try { socket_connect($u, str_repeat('x', 200)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// socket_connect(): Argument #3 ($port) cannot be null when the socket type is AF_INET
/// bool(true)
///
/// Warning: socket_connect(): unable to connect [2]: No such file or directory in %s on line 7
/// bool(false)
/// socket_connect(): Argument #2 ($address) must be less than 104
/// ```
#[php_function]
fn socket_connect(
    req: &mut Request,
    socket: Instance<Socket>,
    address: PhpString,
    #[php(default = null)] port: Option<i64>,
) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    let Some(to) = target(
        req,
        socket,
        address.as_bytes(),
        port,
        3,
        2,
        "be one of AF_UNIX, AF_INET, or AF_INET6",
    )?
    else {
        return Ok(false);
    };
    if let Err(errno) = ffi::connect(fd, &to) {
        socket_error(req, socket.object(), "unable to connect", errno as i32);
        return Ok(false);
    }
    Ok(true)
}

/// `socket_strerror(int $error_code): string`.
///
/// ```php
/// var_dump(socket_strerror(SOCKET_ECONNREFUSED), socket_strerror(-10001), socket_strerror(0));
/// try { socket_strerror(PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(18) "Connection refused"
/// string(12) "Unknown host"
/// string(18) "Undefined error: 0"
/// socket_strerror(): Argument #1 ($error_code) must be between -2147483648 and 2147483647
/// ```
#[php_function]
fn socket_strerror(req: &mut Request, error_code: i64) -> Result<Owner<PhpString>, Thrown> {
    let Ok(code) = i32::try_from(error_code) else {
        return Err(arg_value_error(
            req,
            1,
            "be between -2147483648 and 2147483647",
        ));
    };
    let text = strerror(req, code);
    PhpString::new(req, text.as_bytes())
}

/// `socket_bind(Socket $socket, string $address, int $port = 0): bool`.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// try { socket_bind($s, '127.0.0.1', 70000); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_bind($s, '127.0.0.1'));
/// var_dump(socket_bind(socket_create(AF_INET, SOCK_STREAM, SOL_TCP), 'nonexistent.invalid'));
/// ```
/// ```expect
/// socket_bind(): Argument #3 ($port) must be between 0 and 65535
/// bool(true)
///
/// Warning: socket_bind(): Host lookup failed [-10001]: Unknown host in %s on line 5
/// bool(false)
/// ```
#[php_function]
fn socket_bind(
    req: &mut Request,
    socket: Instance<Socket>,
    address: PhpString,
    #[php(default = 0)] port: i64,
) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    if !(0..=65535).contains(&port) {
        return Err(arg_value_error(req, 3, "be between 0 and 65535"));
    }
    #[cfg(target_os = "linux")]
    if family(req, socket)? == libc::AF_PACKET {
        return bind_packet(req, socket, fd, address.as_bytes());
    }
    let Some(to) = target(
        req,
        socket,
        address.as_bytes(),
        Some(port),
        3,
        2,
        "be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6",
    )?
    else {
        return Ok(false);
    };
    if let Err(errno) = ffi::bind(fd, &to) {
        socket_error(req, socket.object(), "Unable to bind address", errno as i32);
        return Ok(false);
    }
    Ok(true)
}

/// `socket_bind`'s `AF_PACKET` arm: the socket bound to the interface
/// `name` names, up to its first NUL, or to every interface where it
/// names none, as `if_nametoindex`'s `0` binds it; php's `ValueError`
/// where the socket has no name to read.
#[cfg(target_os = "linux")]
fn bind_packet(
    req: &mut Request,
    socket: Instance<Socket>,
    fd: RawFd,
    name: &[u8],
) -> Result<bool, Thrown> {
    let name = c_str(name);
    let index = nix::net::if_::if_nametoindex(name).unwrap_or(0);
    match ffi::bind_packet(fd, index) {
        Ok(()) => Ok(true),
        Err(ffi::PacketBind::Invalid) => {
            Err(throw_error(req, "ValueError", "invalid AF_PACKET socket"))
        }
        Err(ffi::PacketBind::Bind(errno)) => {
            socket_error(req, socket.object(), "Unable to bind address", errno as i32);
            Ok(false)
        }
    }
}

/// `socket_recv(Socket $socket, &$data, int $length, int $flags):
/// int|false`: one `recv(2)`, `$data` the bytes or `null` for none.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// socket_write($p[0], "hello");
/// var_dump(socket_recv($p[1], $d, 3, MSG_PEEK), $d, socket_recv($p[1], $d, 10, 0), $d);
/// socket_close($p[0]);
/// var_dump(socket_recv($p[1], $d, 10, 0), $d, socket_recv($p[1], $d, 0, 0));
/// ```
/// ```expect
/// int(3)
/// string(3) "hel"
/// int(5)
/// string(5) "hello"
/// int(0)
/// NULL
/// bool(false)
/// ```
#[php_function]
fn socket_recv(
    req: &mut Request,
    socket: Instance<Socket>,
    data: Out,
    length: i64,
    flags: i64,
) -> Result<OrFalse<i64>, Thrown> {
    valid(req, socket)?;
    if length <= 0 || length == i64::MAX {
        return Ok(OrFalse::False);
    }
    let mut buf = room(req, usize::try_from(length).unwrap_or(usize::MAX))?;
    let got = wait_on_one(req, socket, |fd| ffi::recv(fd, &mut buf, c_int(flags)))?;
    let value = match got {
        Ok(n) if n > 0 => string(req, &buf[..n])?,
        _ => Owner::null(),
    };
    assign(req, &data, value)?;
    match got {
        Ok(n) => Ok(OrFalse::Value(long(n))),
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "Unable to read from socket",
                errno as i32,
            );
            Ok(OrFalse::False)
        }
    }
}

/// `socket_send(Socket $socket, string $data, int $length, int $flags):
/// int|false`.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// var_dump(socket_send($p[0], "hello", 2, 0), socket_read($p[1], 10));
/// try { socket_send($p[0], "x", -1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// string(2) "he"
/// socket_send(): Argument #3 ($length) must be greater than or equal to 0
/// ```
#[php_function]
fn socket_send(
    req: &mut Request,
    socket: Instance<Socket>,
    data: PhpString,
    length: i64,
    flags: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let fd = valid(req, socket)?;
    if length < 0 {
        return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
    }
    let bytes = data.as_bytes();
    let n = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    match ffi::send(fd, &bytes[..n], c_int(flags)) {
        Ok(sent) => Ok(OrFalse::Value(long(sent))),
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "Unable to write to socket",
                errno as i32,
            );
            Ok(OrFalse::False)
        }
    }
}

/// `socket_recvfrom(Socket $socket, &$data, int $length, int $flags, &$address, &$port = null): int|false`.
///
/// ```php
/// $a = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP); socket_bind($a, '127.0.0.1'); socket_getsockname($a, $ip, $port);
/// $b = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP); socket_bind($b, '127.0.0.1'); socket_getsockname($b, $ip, $bport);
/// var_dump(socket_sendto($b, "ping", 4, 0, '127.0.0.1', $port));
/// var_dump(socket_recvfrom($a, $d, 10, 0, $from, $p), $d, $from, $p === $bport);
/// try { socket_recvfrom($a, $d, 10, 0, $from); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_recvfrom($a, $d, 0, 0, $from));
/// ```
/// ```expect
/// int(4)
/// int(4)
/// string(4) "ping"
/// string(9) "127.0.0.1"
/// bool(true)
/// Wrong parameter count for socket_recvfrom()
/// bool(false)
/// ```
#[php_function]
fn socket_recvfrom(
    req: &mut Request,
    socket: Instance<Socket>,
    data: Out,
    length: i64,
    flags: i64,
    address: Out,
    #[php(default = null)] port: Out,
) -> Result<OrFalse<i64>, Thrown> {
    valid(req, socket)?;
    if length <= 0 || length > i64::MAX - 1 {
        return Ok(OrFalse::False);
    }
    let family = family(req, socket)?;
    if !inet_family(i64::from(family)) {
        return Err(arg_value_error(
            req,
            1,
            "be one of AF_UNIX, AF_INET, or AF_INET6",
        ));
    }
    let mut buf = room(req, usize::try_from(length).unwrap_or(usize::MAX) + 1)?;
    buf.pop();
    if family != libc::AF_UNIX && !port.is_given() {
        return Err(throw_error(
            req,
            "ArgumentCountError",
            "Wrong parameter count for socket_recvfrom()",
        ));
    }
    let received = wait_on_one(req, socket, |fd| {
        ffi::recvfrom(fd, &mut buf, c_int(flags), family)
    })?;
    let (n, from) = match received {
        Ok(got) => got,
        Err(errno) => {
            let msg = if family == libc::AF_INET6 {
                "unable to recvfrom"
            } else {
                "Unable to recvfrom"
            };
            socket_error(req, socket.object(), msg, errno as i32);
            return Ok(OrFalse::False);
        }
    };
    let bytes = string(req, &buf[..n])?;
    assign(req, &data, bytes)?;
    let (text, at) = match (&from, family) {
        (Addr::V4(v4), libc::AF_INET) => (ntop(&from), Some(v4.port())),
        (Addr::V6(v6), libc::AF_INET6) => (ntop(&from), Some(v6.port())),
        (Addr::Unix(path), _) => (path.clone(), None),
        (_, libc::AF_INET) => (b"0.0.0.0".to_vec(), Some(0)),
        (_, libc::AF_INET6) => (b"::".to_vec(), Some(0)),
        _ => (Vec::new(), None),
    };
    let text = string(req, &text)?;
    assign(req, &address, text)?;
    if let Some(at) = at {
        assign(req, &port, Value::int(i64::from(at)))?;
    }
    Ok(OrFalse::Value(long(n)))
}

/// `socket_sendto(Socket $socket, string $data, int $length, int $flags, string $address, ?int $port = null): int|false`.
///
/// ```php
/// $a = socket_create(AF_UNIX, SOCK_DGRAM, 0);
/// $path = sys_get_temp_dir() . '/sendto' . getmypid() . '.sock';
/// @unlink($path); socket_bind($a, $path);
/// $b = socket_create(AF_UNIX, SOCK_DGRAM, 0);
/// var_dump(socket_sendto($b, "hello", 3, 0, $path), socket_recvfrom($a, $d, 10, 0, $from), $d, $from);
/// try { socket_sendto($b, "x", 1, 0, $path, 70000); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $i = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP);
/// try { socket_sendto($i, "x", 1, 0, '127.0.0.1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($path);
/// ```
/// ```expect
/// int(3)
/// int(3)
/// string(3) "hel"
/// string(0) ""
/// socket_sendto(): Argument #6 ($port) must be between 0 and 65535
/// socket_sendto(): Argument #6 ($port) cannot be null when the socket type is AF_INET
/// ```
#[php_function]
fn socket_sendto(
    req: &mut Request,
    socket: Instance<Socket>,
    data: PhpString,
    length: i64,
    flags: i64,
    address: PhpString,
    #[php(default = null)] port: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let fd = valid(req, socket)?;
    if port.is_some_and(|p| !(0..=65535).contains(&p)) {
        return Err(arg_value_error(req, 6, "be between 0 and 65535"));
    }
    if length < 0 {
        return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
    }
    let family = family(req, socket)?;
    let address = if family == libc::AF_UNIX {
        if address.as_bytes().len() >= SUN_PATH {
            let tail = format!("be less than {SUN_PATH}");
            return Err(arg_value_error(req, 5, &tail));
        }
        c_str(address.as_bytes()).to_vec()
    } else {
        address.as_bytes().to_vec()
    };
    let Some(to) = target(
        req,
        socket,
        &address,
        port,
        6,
        5,
        "be one of AF_UNIX, AF_INET, or AF_INET6",
    )?
    else {
        return Ok(OrFalse::False);
    };
    let bytes = data.as_bytes();
    let n = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    match ffi::sendto(fd, &bytes[..n], c_int(flags), &to) {
        Ok(sent) => Ok(OrFalse::Value(long(sent))),
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "Unable to write to socket",
                errno as i32,
            );
            Ok(OrFalse::False)
        }
    }
}

/// `socket_create_pair(int $domain, int $type, int $protocol, &$pair):
/// bool`: two connected sockets in `$pair`.
///
/// ```php
/// var_dump(socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p), count($p), $p[0] instanceof Socket);
/// var_dump(socket_create_pair(AF_INET, SOCK_STREAM, 0, $q));
/// try { socket_create_pair(99, SOCK_STREAM, 0, $q); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// bool(true)
///
/// Warning: socket_create_pair(): Unable to create socket pair [102]: Operation not supported on socket in %s on line 3
/// bool(false)
/// socket_create_pair(): Argument #1 ($domain) must be one of AF_UNIX, AF_INET6, or AF_INET
/// ```
#[php_function]
fn socket_create_pair(
    req: &mut Request,
    domain: i64,
    r#type: i64,
    protocol: i64,
    pair: Out,
) -> Result<bool, Thrown> {
    if !inet_family(domain) {
        return Err(arg_value_error(
            req,
            1,
            "be one of AF_UNIX, AF_INET6, or AF_INET",
        ));
    }
    check_type(req, r#type)?;
    let fds = match ffi::socketpair(c_int(domain), c_int(r#type), c_int(protocol)) {
        Ok(fds) => fds,
        Err(errno) => {
            global_error(req, "Unable to create socket pair", errno as i32);
            return Ok(false);
        }
    };
    let empty = PhpArray::new(req)?;
    assign(req, &pair, empty.into_value())?;
    let mut out = PhpArray::new(req)?;
    for fd in fds {
        let made = new_socket(req, Sock::owning(fd, c_int(domain)))?;
        out.append(req, made.into_value())?;
    }
    assign(req, &pair, out.into_value())?;
    Ok(true)
}

/// `socket_shutdown(Socket $socket, int $mode = 2): bool`.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// var_dump(socket_shutdown($p[0], SHUT_WR), socket_read($p[1], 10));
/// try { socket_shutdown($p[0], 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// socket_shutdown(): Argument #2 ($mode) must be one of SHUT_RD, SHUT_WR or SHUT_RDWR
/// ```
#[php_function]
fn socket_shutdown(
    req: &mut Request,
    socket: Instance<Socket>,
    #[php(default = 2)] mode: i64,
) -> Result<bool, Thrown> {
    use nix::sys::socket::{Shutdown, shutdown};
    let fd = valid(req, socket)?;
    let how = match mode {
        0 => Shutdown::Read,
        1 => Shutdown::Write,
        2 => Shutdown::Both,
        _ => {
            return Err(arg_value_error(
                req,
                2,
                "be one of SHUT_RD, SHUT_WR or SHUT_RDWR",
            ));
        }
    };
    if let Err(errno) = shutdown(fd, how) {
        socket_error(
            req,
            socket.object(),
            "Unable to shutdown socket",
            errno as i32,
        );
        return Ok(false);
    }
    Ok(true)
}

/// `socket_atmark(Socket $socket): bool`: whether the socket is at the
/// out-of-band mark.
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
/// var_dump(socket_atmark($p[0]));
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn socket_atmark(req: &mut Request, socket: Instance<Socket>) -> Result<bool, Thrown> {
    let fd = valid(req, socket)?;
    match ffi::at_mark(fd) {
        Ok(at) => Ok(at),
        Err(errno) => {
            socket_error(
                req,
                socket.object(),
                "Unable to apply sockmark",
                errno as i32,
            );
            Ok(false)
        }
    }
}

/// `socket_last_error(?Socket $socket = null): int`: the socket's last
/// error, or the extension's.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// @socket_connect($s, '127.0.0.1', 1);
/// var_dump(socket_last_error($s) === SOCKET_ECONNREFUSED, socket_last_error() === SOCKET_ECONNREFUSED);
/// socket_clear_error($s);
/// var_dump(socket_last_error($s), socket_last_error() === SOCKET_ECONNREFUSED);
/// socket_clear_error();
/// var_dump(socket_last_error());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// int(0)
/// bool(true)
/// int(0)
/// ```
#[php_function]
fn socket_last_error(
    req: &mut Request,
    #[php(default = null)] socket: Option<Instance<Socket>>,
) -> Result<i64, Thrown> {
    match socket {
        Some(socket) => {
            valid(req, socket)?;
            Ok(i64::from(sock(req, socket)?.error))
        }
        None => Ok(i64::from(req.state::<State>().last_error)),
    }
}

/// `socket_clear_error(?Socket $socket = null): void`.
///
/// ```php
/// $s = socket_create(AF_INET, SOCK_STREAM, SOL_TCP);
/// socket_close($s);
/// try { socket_clear_error($s); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// socket_clear_error(): Argument #1 ($socket) has already been closed
/// ```
#[php_function]
fn socket_clear_error(
    req: &mut Request,
    #[php(default = null)] socket: Option<Instance<Socket>>,
) -> Result<(), Thrown> {
    match socket {
        Some(socket) => {
            valid(req, socket)?;
            sock(req, socket)?.error = 0;
        }
        None => req.state::<State>().last_error = 0,
    }
    Ok(())
}

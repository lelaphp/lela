//! `sendrecvmsg.c`'s natives: `socket_sendmsg`, `socket_recvmsg` and
//! `socket_cmsg_space`, over [`crate::conv`].

use php_ext::abi::{Instance, OrFalse, Ref};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray};

use crate::State;
use crate::conv::{Ancillary, Conv};
use crate::ffi::{self, cmsg};
use crate::natives::long;
use crate::socket::{Socket, Waited, closed, sock, socket_error, strerror, valid, wait_on_socket};

/// `LONG_CHECK_VALID_INT`: php's `ValueError` for a number no C `int`
/// holds.
fn int_arg(req: &mut Request, n: i64, position: u32) -> Result<i32, Thrown> {
    i32::try_from(n)
        .map_err(|_| arg_value_error(req, position, "be between -2147483648 and 2147483647"))
}

/// `socket_sendmsg(Socket $socket, array $message, int $flags = 0):
/// int|false`: the message's `name`, `iov` and `control` laid out and
/// handed to `sendmsg(2)`.
///
/// ```php
/// $a = socket_create(AF_INET6, SOCK_DGRAM, SOL_UDP); socket_bind($a, '::1'); socket_getsockname($a, $ip, $port);
/// $b = socket_create(AF_INET6, SOCK_DGRAM, SOL_UDP);
/// var_dump(socket_sendmsg($b, ['name' => ['addr' => '::1', 'port' => $port], 'iov' => ['he', 'llo']], 0));
/// var_dump(socket_recvfrom($a, $d, 10, 0, $from, $p), $d);
/// var_dump(socket_sendmsg($b, ['name' => ['addr' => '::1', 'port' => 'x']]));
/// var_dump(socket_sendmsg($b, ['control' => [['level' => 1, 'type' => 2]]]));
/// ```
/// ```expect
/// int(5)
/// int(5)
/// string(5) "hello"
///
/// Warning: socket_sendmsg(): error converting user data (path: msghdr > name > port): expected an integer, but got a non numeric string (possibly from a converted object): 'x' in %s on line 6
/// bool(false)
///
/// Warning: socket_sendmsg(): error converting user data (path: msghdr > control > element #0): cmsghdr with level 1 and type 2 not supported in %s on line 7
/// bool(false)
/// ```
///
/// An fd goes as `SCM_RIGHTS` and comes back a `Socket`:
///
/// ```php
/// socket_create_pair(AF_UNIX, SOCK_DGRAM, 0, $p);
/// socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $q);
/// var_dump(socket_sendmsg($p[0], ['iov' => ['x'], 'control' => [['level' => SOL_SOCKET, 'type' => SCM_RIGHTS, 'data' => [$q[0]]]]]));
/// $m = ['controllen' => socket_cmsg_space(SOL_SOCKET, SCM_RIGHTS, 1)];
/// var_dump(socket_recvmsg($p[1], $m), $m['iov'], $m['control'][0]['data'][0] instanceof Socket);
/// socket_write($m['control'][0]['data'][0], "via");
/// var_dump(socket_read($q[1], 10));
/// ```
/// ```expect
/// int(1)
/// int(1)
/// array(1) {
///   [0]=>
///   string(1) "x"
/// }
/// bool(true)
/// string(3) "via"
/// ```
#[php_function]
fn socket_sendmsg(
    req: &mut Request,
    socket: Instance<Socket>,
    message: PhpArray,
    #[php(default = 0)] flags: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let flags = int_arg(req, flags, 3)?;
    let fd = valid(req, socket)?;
    let family = sock(req, socket)?.family;
    let mut conv = Conv::user("msghdr", socket.object(), family);
    let send = conv.msghdr_send(req, message)?;
    if conv.failed() {
        conv.report(req);
        return Ok(OrFalse::False);
    }
    match ffi::sendmsg(fd, send.name.as_ref(), &send.iov, &send.control, flags) {
        Ok(n) => Ok(OrFalse::Value(long(n))),
        Err(errno) => {
            socket_error(req, socket.object(), "Error in sendmsg", errno as i32);
            Ok(OrFalse::False)
        }
    }
}

/// `socket_recvmsg(Socket $socket, array &$message, int $flags = 0):
/// int|false`: room laid out as the message's `name`, `buffer_size` and
/// `controllen` ask, `recvmsg(2)`, and the message replaced by what came:
/// its `name`, `control`, `iov` and `flags`.
///
/// ```php
/// $a = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP); socket_bind($a, '127.0.0.1'); socket_getsockname($a, $ip, $port);
/// $b = socket_create(AF_INET, SOCK_DGRAM, SOL_UDP); socket_bind($b, '127.0.0.1'); socket_getsockname($b, $ip, $bport);
/// socket_sendto($b, "hello", 5, 0, '127.0.0.1', $port);
/// $m = ['name' => [], 'buffer_size' => 3, 'controllen' => 1];
/// var_dump(socket_recvmsg($a, $m, 0), $m['name']['addr'], $m['name']['port'] === $bport, $m['iov'], $m['flags'] === MSG_TRUNC);
/// $m = [];
/// var_dump(socket_recvmsg($a, $m));
/// try { socket_recvmsg($a, $m, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// string(9) "127.0.0.1"
/// bool(true)
/// array(1) {
///   [0]=>
///   string(3) "hel"
/// }
/// bool(true)
///
/// Warning: socket_recvmsg(): error converting user data (path: msghdr): The key 'controllen' is required in %s on line 8
/// bool(false)
/// socket_recvmsg(): Argument #3 ($flags) must be between -2147483648 and 2147483647
/// ```
#[php_function]
fn socket_recvmsg(
    req: &mut Request,
    socket: Instance<Socket>,
    message: Ref<PhpArray>,
    #[php(default = 0)] flags: i64,
) -> Result<OrFalse<i64>, Thrown> {
    let flags = int_arg(req, flags, 3)?;
    valid(req, socket)?;
    let family = sock(req, socket)?.family;
    let Some(array) = message.get().copied() else {
        return Ok(OrFalse::False);
    };
    let mut conv = Conv::user("msghdr", socket.object(), family);
    let room = conv.msghdr_recv(req, array)?;
    if conv.failed() {
        conv.report(req);
        return Ok(OrFalse::False);
    }
    let waited = wait_on_socket(req, socket, |fd| {
        ffi::recvmsg(fd, room.name, room.size, room.controllen, flags)
    });
    let received = match waited {
        Waited::Answered(received) => received,
        Waited::Closed { answered, .. } => {
            // The message is let go unread, and the fds it carried with it.
            if let Some(Ok(got)) = answered {
                drop(got.rights());
            }
            return Err(closed(req, 1));
        }
    };
    let got = match received {
        Ok(got) => got,
        Err(errno) => {
            let code = errno as i32;
            req.state::<State>().last_error = code;
            let text = format!("Error in recvmsg [{code}]: {}", strerror(req, code));
            docref(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    };
    let mut conv = Conv::native("msghdr", socket.object(), family);
    let value = conv.msghdr_value(req, &got, room.size)?;
    let value = if conv.failed() {
        value.give_up(req);
        conv.report(req);
        Owner::bool(false)
    } else {
        value
    };
    match message.reference() {
        Some(cell) => cell.set(req, value)?,
        None => value.give_up(req),
    }
    Ok(OrFalse::Value(long(got.count)))
}

/// `socket_cmsg_space(int $level, int $type, int $num = 0): ?int`:
/// `CMSG_SPACE` of the data one `(level, type)` pair carries, `$num`
/// elements of it where it is a sequence.
///
/// ```php
/// var_dump(socket_cmsg_space(IPPROTO_IPV6, IPV6_PKTINFO), socket_cmsg_space(IPPROTO_IPV6, IPV6_HOPLIMIT), socket_cmsg_space(SOL_SOCKET, SCM_RIGHTS, 3));
/// try { socket_cmsg_space(1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { socket_cmsg_space(SOL_SOCKET, SCM_RIGHTS, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(socket_cmsg_space(SOL_SOCKET, SCM_RIGHTS, PHP_INT_MAX >> 33));
/// ```
/// ```expect
/// int(32)
/// int(16)
/// int(24)
/// Pair level 1 and/or type 2 is not supported
/// socket_cmsg_space(): Argument #3 ($num) must be greater than or equal to 0
/// int(4294967304)
/// ```
#[php_function]
fn socket_cmsg_space(
    req: &mut Request,
    level: i64,
    r#type: i64,
    #[php(default = 0)] num: i64,
) -> Result<Option<i64>, Thrown> {
    let level = int_arg(req, level, 1)?;
    let ty = int_arg(req, r#type, 2)?;
    int_arg(req, num, 3)?;
    if num < 0 {
        return Err(arg_value_error(req, 3, "be greater than or equal to 0"));
    }
    let Some(entry) = Ancillary::of(level, ty) else {
        let text = format!("Pair level {level} and/or type {ty} is not supported");
        return Err(value_error(req, &text));
    };
    let n = usize::try_from(num).unwrap_or(0);
    let size = entry.size() + n * entry.element();
    let space = cmsg::space(size);
    if entry.element() > 0 && i64::try_from(space).is_err() {
        return Err(arg_value_error(req, 3, "is too large"));
    }
    Ok(Some(long(space)))
}

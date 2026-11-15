//! The `ftp` extension: `php-src/ext/ftp` as one crate under `ext/`
//! (ADR-0004). One workspace dependency, the facade; the sockets are the
//! engine's, and FTPS is the crypto layer the bundle's `openssl`
//! registers, which the `ftp` feature turns on; one entry,
//! `extension()`. The phpt subset is `php-src/ext/ftp/tests`, through
//! `xtask phpt --subset`.
//!
//! Every native is `php_ftp.c`'s at the tag, in `ftp.stub.php`'s order,
//! over [`ftp`], which is `ftp.c`. An `FTP\Connection` holds its session
//! (`php_ftp_object`'s `ftpbuf_t`), lends it to one native at a time,
//! and holds nothing once closed.
//!
//! The witnesses serve themselves: each forks a server that greets, then
//! answers each command line it reads with the next reply it was given,
//! and, for a transfer, opens a passive data connection of its own.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ftp;
mod throwables;

use php_ext::abi::{
    Extension, ExtensionBuilder, GcTable, Instance, ObjectHandlers, OrFalse, Path, Ref, Refusals,
    True, Union, Untyped,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{c_str, value_name};
use php_ext::raising::{
    Level, Thrown, arg_type_error, arg_value_error, docref, throw, throw_error,
};
use php_ext::request::{OwnedStream as _, Request, Stream, Streams as _, Whence};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::ftp::{Ftp, Kind, Progress, Verb};
use crate::throwables::Throwables;

/// The constants `register_ftp_symbols` registers.
///
/// ```php
/// var_dump(FTP_ASCII, FTP_TEXT, FTP_BINARY, FTP_IMAGE, FTP_AUTORESUME, FTP_TIMEOUT_SEC, FTP_AUTOSEEK, FTP_USEPASVADDRESS, FTP_FAILED, FTP_FINISHED, FTP_MOREDATA);
/// ```
/// ```expect
/// int(1)
/// int(1)
/// int(2)
/// int(2)
/// int(-1)
/// int(0)
/// int(1)
/// int(2)
/// int(0)
/// int(1)
/// int(2)
/// ```
#[php_const]
mod constants {
    pub(crate) const FTP_ASCII: i64 = 1;
    pub(crate) const FTP_TEXT: i64 = 1;
    pub(crate) const FTP_BINARY: i64 = 2;
    pub(crate) const FTP_IMAGE: i64 = 2;
    pub(crate) const FTP_AUTORESUME: i64 = -1;
    pub(crate) const FTP_TIMEOUT_SEC: i64 = 0;
    pub(crate) const FTP_AUTOSEEK: i64 = 1;
    pub(crate) const FTP_USEPASVADDRESS: i64 = 2;
    pub(crate) const FTP_FAILED: i64 = 0;
    pub(crate) const FTP_FINISHED: i64 = 1;
    pub(crate) const FTP_MOREDATA: i64 = 2;
}

/// `PHP_FTP_AUTORESUME`.
const AUTORESUME: i64 = -1;

/// The extension value (design §7.1): the constants, the natives and the
/// class.
///
/// ```php
/// var_dump(extension_loaded('ftp'), function_exists('ftp_ssl_connect'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::DECLARED)
        .functions(&[
            ftp_connect::DECLARED,
            ftp_ssl_connect::DECLARED,
            ftp_login::DECLARED,
            ftp_pwd::DECLARED,
            ftp_cdup::DECLARED,
            ftp_chdir::DECLARED,
            ftp_exec::DECLARED,
            ftp_raw::DECLARED,
            ftp_mkdir::DECLARED,
            ftp_rmdir::DECLARED,
            ftp_chmod::DECLARED,
            ftp_alloc::DECLARED,
            ftp_nlist::DECLARED,
            ftp_rawlist::DECLARED,
            ftp_mlsd::DECLARED,
            ftp_systype::DECLARED,
            ftp_fget::DECLARED,
            ftp_nb_fget::DECLARED,
            ftp_pasv::DECLARED,
            ftp_get::DECLARED,
            ftp_nb_get::DECLARED,
            ftp_nb_continue::DECLARED,
            ftp_fput::DECLARED,
            ftp_nb_fput::DECLARED,
            ftp_put::DECLARED,
            ftp_append::DECLARED,
            ftp_nb_put::DECLARED,
            ftp_size::DECLARED,
            ftp_mdtm::DECLARED,
            ftp_rename::DECLARED,
            ftp_delete::DECLARED,
            ftp_site::DECLARED,
            ftp_close::DECLARED,
            ftp_quit::DECLARED,
            ftp_set_option::DECLARED,
            ftp_get_option::DECLARED,
        ])
        .class::<Connection>()
        .engine_state(Throwables::start)
        .info(ftp_info)
        .build("ftp")
}

/// `PHP_MINFO_FUNCTION(ftp)`.
///
/// ```php
/// (new ReflectionExtension('ftp'))->info();
/// ```
/// ```expect
/// ftp
///
/// FTP support => enabled
/// FTPS support => enabled
/// ```
fn ftp_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["FTP support", "enabled"], &["FTPS support", "enabled"]]);
}

/// Where a connection's session is: open, lent to the native serving it
/// (php's `in_use` for a transfer), or closed (`ftp == NULL`).
/// Boxed as the payload, so the size of `Open` costs the object nothing.
#[derive(Default)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Conn {
    #[default]
    Closed,
    Open(Ftp),
    Lent,
}

/// `final class FTP\Connection`, `@strict-properties`,
/// `@not-serializable`.
///
/// ```php
/// try { new FTP\Connection; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump((new ReflectionClass('FTP\Connection'))->isFinal());
/// ```
/// ```expect
/// Cannot directly construct FTP\Connection, use ftp_connect() or ftp_ssl_connect() instead
/// bool(true)
/// ```
#[php_class(
    final,
    namespace(FTP),
    not_serializable,
    size = 64, payload = Box<Conn>,
    handlers = FTP,
    dynamic_properties = forbidden
)]
pub(crate) struct Connection;

#[php_class]
impl Connection {}

/// `ftp_object_handlers`: `get_constructor` refusing, `clone_obj` php's
/// null slot, and `free_obj` closing the session. `compare` stays
/// `std_object_handlers`' `zend_std_compare_objects`: two connections
/// hold no properties, so they are equal, open or closed, and
/// uncomparable with an object of another class.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { foreach ([1, 2] as $_) { $c[] = $x = stream_socket_accept($s); fwrite($x, "220 hi\r\n"); } exit; }
/// $a = ftp_connect("127.0.0.1", $p); $b = ftp_connect("127.0.0.1", $p);
/// var_dump($a == $b, $a != $b, $a < $b, $a <=> $b, $a === $b, $a == $a, $a <=> $a, $a === $a);
/// var_dump(in_array($b, [$a]), in_array($b, [$a], true), $a == new stdClass, $a <=> new stdClass);
/// ftp_close($b); var_dump($a == $b, $a <=> $b);
/// ```
/// ```expect process
/// bool(true)
/// bool(false)
/// bool(false)
/// int(0)
/// bool(false)
/// bool(true)
/// int(0)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// int(1)
/// bool(true)
/// int(0)
/// ```
pub(crate) struct Handlers;

pub(crate) static FTP: Handlers = Handlers;

/// What `ftp_object_handlers` refuses: construction and cloning.
const REFUSALS: Refusals = Refusals::NONE
    .construction(Some("ftp_connect() or ftp_ssl_connect()"))
    .cloning();

impl ObjectHandlers for Handlers {
    /// `ftp_object_destroy`: `ftp_close` of a session still open.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let conn = object
            .native_mut::<Box<Conn>>(req)
            .map(|conn| std::mem::take(&mut **conn));
        if let Some(Conn::Open(ftp)) = conn {
            ftp.close(req);
        }
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
        if let Some(conn) = object.native::<Box<Conn>>(req)
            && let Conn::Open(ftp) = &**conn
        {
            for stream in ftp.streams() {
                visit(stream);
            }
        }
        GcTable::Properties
    }
}

/// `GET_FTPBUF`: the `ValueError` for a closed connection.
fn closed(req: &mut Request) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw(req, ce.value_error, "FTP\\Connection is already closed")
}

/// What lending a session came to: `f`'s answer, or `None` where it is
/// lent already, after php's warning that it is in use.
fn lend<R>(
    req: &mut Request,
    ftp: Instance<Connection>,
    f: impl FnOnce(&mut Request, &mut Ftp) -> Result<R, Thrown>,
) -> Result<Option<R>, Thrown> {
    let conn = std::mem::replace(&mut **ftp.payload_mut(req)?, Conn::Lent);
    match conn {
        Conn::Open(mut session) => {
            let answer = f(req, &mut session);
            match ftp.payload_mut(req) {
                Ok(conn) => **conn = Conn::Open(session),
                Err(thrown) => {
                    session.close(req);
                    return Err(thrown);
                }
            }
            answer.map(Some)
        }
        Conn::Closed => {
            **ftp.payload_mut(req)? = Conn::Closed;
            Err(closed(req))
        }
        Conn::Lent => {
            docref(req, Level::Warning, "FTP\\Connection is already in use");
            Ok(None)
        }
    }
}

/// `GET_FTPBUF` alone: the `ValueError` for a closed connection, before
/// a native reads its other arguments.
fn open(req: &mut Request, ftp: Instance<Connection>) -> Result<(), Thrown> {
    if matches!(**ftp.payload_mut(req)?, Conn::Closed) {
        return Err(closed(req));
    }
    Ok(())
}

/// The session's autoseek, `true` while it is lent.
fn autoseek(req: &mut Request, ftp: Instance<Connection>) -> Result<bool, Thrown> {
    Ok(match &**ftp.payload_mut(req)? {
        Conn::Open(session) => session.autoseek,
        _ => true,
    })
}

/// `php_error_docref(NULL, E_WARNING, "%s", ftp->inbuf)` where the last
/// reply left text.
fn warn_reply(req: &mut Request, ftp: &Ftp) {
    let text = c_str(&ftp.inbuf);
    if !text.is_empty() {
        let text = String::from_utf8_lossy(text).into_owned();
        docref(req, Level::Warning, &text);
    }
}

/// A native of one command that answers `bool`, the reply's text warned
/// where it fails.
fn simple(
    req: &mut Request,
    ftp: Instance<Connection>,
    f: impl FnOnce(&mut Request, &mut Ftp) -> Result<bool, Thrown>,
) -> Result<bool, Thrown> {
    Ok(lend(req, ftp, |req, session| {
        let done = f(req, session)?;
        if !done {
            warn_reply(req, session);
        }
        Ok(done)
    })?
    .unwrap_or(false))
}

/// `XTYPE`: `FTP_ASCII` or `FTP_BINARY`, the `ValueError` for any other.
fn xtype(req: &mut Request, mode: i64) -> Result<Kind, Thrown> {
    match mode {
        1 => Ok(Kind::Ascii),
        2 => Ok(Kind::Image),
        _ => Err(arg_value_error(req, 4, "be either FTP_ASCII or FTP_BINARY")),
    }
}

/// `PHP_FTP_TIMEOUT_SEC_MAX`, `PHP_TIMEOUT_ULL_MAX / 1000000.0` as a
/// `uint64_t`: the seconds past which the timeout's microseconds overflow.
const TIMEOUT_SEC_MAX: i64 = 18_446_744_073_709;

/// Where a timeout in seconds is given: a connect's `$timeout`, or
/// `ftp_set_option`'s `$value` for `FTP_TIMEOUT_SEC`, whose refusals say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutFrom {
    Connect,
    SetOption,
}

impl TimeoutFrom {
    /// What ends each refusal's sentence.
    const fn tail(self) -> &'static str {
        match self {
            TimeoutFrom::Connect => "",
            TimeoutFrom::SetOption => " for the FTP_TIMEOUT_SEC option",
        }
    }
}

/// `timeout_sec` held above zero and below [`TIMEOUT_SEC_MAX`], as
/// argument 3 (php 8.5.12's GH-20601: `ftp_ssl_connect` and
/// `ftp_set_option` check the bound too).
fn bounded_timeout(req: &mut Request, timeout: i64, from: TimeoutFrom) -> Result<(), Thrown> {
    let tail = from.tail();
    if timeout <= 0 {
        return Err(arg_value_error(req, 3, &format!("be greater than 0{tail}")));
    }
    if timeout >= TIMEOUT_SEC_MAX {
        let text = format!("be less than {TIMEOUT_SEC_MAX}{tail}");
        return Err(arg_value_error(req, 3, &text));
    }
    Ok(())
}

/// The new connection over an open session.
fn connection(req: &mut Request, session: Ftp) -> Result<Owner<Instance<Connection>>, Thrown> {
    let Some(class) = lookup_class(req, b"FTP\\Connection")? else {
        session.close(req);
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "Class \"FTP\\Connection\" not found",
        ));
    };
    let ftp = new_object(req, class)?.into_instance::<Connection>();
    match ftp.payload_mut(req) {
        Ok(payload) => **payload = Conn::Open(session),
        Err(thrown) => {
            ftp.give_up(req);
            return Err(thrown);
        }
    }
    Ok(ftp)
}

/// `ftp_connect(string $hostname, int $port = 21, int $timeout = 90):`
/// `FTP\Connection|false`: the control connection and its `220`.
///
/// ```php
/// var_dump(ftp_connect('127.0.0.1', 1));
/// try { ftp_connect('x', 21, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { ftp_connect('x', 21, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// ftp_connect(): Argument #3 ($timeout) must be greater than 0
/// ftp_connect(): Argument #3 ($timeout) must be less than 18446744073709
/// ```
#[php_function]
fn ftp_connect(
    req: &mut Request,
    hostname: PhpString,
    #[php(default = 21)] port: i64,
    #[php(default = 90)] timeout: i64,
) -> Result<OrFalse<Owner<Instance<Connection>>>, Thrown> {
    bounded_timeout(req, timeout, TimeoutFrom::Connect)?;
    match Ftp::open(req, hostname.as_bytes(), port, timeout, false)? {
        Some(session) => Ok(OrFalse::Value(connection(req, session)?)),
        None => Ok(OrFalse::False),
    }
}

/// `ftp_ssl_connect(string $hostname, int $port = 21, int $timeout = 90):`
/// `FTP\Connection|false`: as `ftp_connect`, FTPS asked for at login.
///
/// ```php
/// var_dump(ftp_ssl_connect('127.0.0.1', 1));
/// try { ftp_ssl_connect('x', 21, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { ftp_ssl_connect('x', 21, PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// ftp_ssl_connect(): Argument #3 ($timeout) must be greater than 0
/// ftp_ssl_connect(): Argument #3 ($timeout) must be less than 18446744073709
/// ```
#[php_function]
fn ftp_ssl_connect(
    req: &mut Request,
    hostname: PhpString,
    #[php(default = 21)] port: i64,
    #[php(default = 90)] timeout: i64,
) -> Result<OrFalse<Owner<Instance<Connection>>>, Thrown> {
    bounded_timeout(req, timeout, TimeoutFrom::Connect)?;
    match Ftp::open(req, hostname.as_bytes(), port, timeout, true)? {
        Some(session) => Ok(OrFalse::Value(connection(req, session)?)),
        None => Ok(OrFalse::False),
    }
}

/// `ftp_login(FTP\Connection $ftp, string $username,`
/// `#[\SensitiveParameter] string $password): bool`.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["331 pass?", "530 no", "331 pass?", "230 in"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_login($f, "u", "bad"), ftp_login($f, "u", "good"));
/// ```
/// ```expect process
/// Warning: ftp_login(): no in %s on line 5
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn ftp_login(
    req: &mut Request,
    ftp: Instance<Connection>,
    username: PhpString,
    #[php(sensitive)] password: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.login(req, username.as_bytes(), password.as_bytes())
    })
}

/// A native that answers a string or `false`, the reply's text warned
/// where it fails.
fn text_or_false(
    req: &mut Request,
    ftp: Instance<Connection>,
    f: impl FnOnce(&mut Request, &mut Ftp) -> Result<Option<Vec<u8>>, Thrown>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let text = lend(req, ftp, |req, session| {
        let text = f(req, session)?;
        if text.is_none() {
            warn_reply(req, session);
        }
        Ok(text)
    })?
    .flatten();
    Ok(match text {
        Some(text) => OrFalse::Value(PhpString::new(req, &text)?),
        None => OrFalse::False,
    })
}

/// `ftp_pwd(FTP\Connection $ftp): string|false`: the `257` reply's
/// quoted path, cached until the directory changes.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["257 \"/a\" is it", "250 ok", "550 no such", "257 \"/b\"", "250 ok", "500 what"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_pwd($f), ftp_pwd($f), ftp_chdir($f, "b"), ftp_chdir($f, "x"), ftp_pwd($f), ftp_cdup($f), ftp_exec($f, "ls"));
/// ```
/// ```expect process
/// Warning: ftp_chdir(): no such in %s on line 5
///
/// Warning: ftp_exec(): what in %s on line 5
/// string(2) "/a"
/// string(2) "/a"
/// bool(true)
/// bool(false)
/// string(2) "/b"
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_pwd(
    req: &mut Request,
    ftp: Instance<Connection>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    text_or_false(req, ftp, |req, session| session.pwd(req))
}

/// `ftp_cdup(FTP\Connection $ftp): bool`: `CDUP`.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["250 up", "550 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_cdup($f), ftp_cdup($f));
/// ```
/// ```expect process
/// Warning: ftp_cdup(): no in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_cdup(req: &mut Request, ftp: Instance<Connection>) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Cdup, b"")
    })
}

/// `ftp_chdir(FTP\Connection $ftp, string $directory): bool`: `CWD`,
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["250 ok"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_chdir($f, "d"), ftp_chdir($f, "a\r\nb"));
/// ```
/// ```expect process
/// Warning: ftp_chdir(): ok in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_chdir(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Chdir, directory.as_bytes())
    })
}

/// `ftp_exec(FTP\Connection $ftp, string $command): bool`: `SITE EXEC`,
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["200 done", "500 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_exec($f, "ls"), ftp_exec($f, "rm"));
/// ```
/// ```expect process
/// Warning: ftp_exec(): no in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_exec(
    req: &mut Request,
    ftp: Instance<Connection>,
    command: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Exec, command.as_bytes())
    })
}

/// A list of lines as an array of strings.
fn lines_array(req: &mut Request, lines: Vec<Vec<u8>>) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for line in lines {
        let line = PhpString::new(req, &line)?;
        array.append(req, line.into_value())?;
    }
    Ok(array)
}

/// `ftp_raw(FTP\Connection $ftp, string $command): ?array`: every line
/// of the reply, `null` where the command could not be sent.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); fgets($c); fwrite($c, "214-help\r\n x\r\n214 end\r\n"); fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_raw($f, "HELP"), ftp_raw($f, "A\r\nB"));
/// ```
/// ```expect process
/// array(3) {
///   [0]=>
///   string(8) "214-help"
///   [1]=>
///   string(2) " x"
///   [2]=>
///   string(7) "214 end"
/// }
/// NULL
/// ```
#[php_function]
fn ftp_raw(
    req: &mut Request,
    ftp: Instance<Connection>,
    command: PhpString,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let lines = lend(req, ftp, |req, session| {
        session.raw(req, command.as_bytes())
    })?
    .flatten();
    match lines {
        Some(lines) => Ok(Some(lines_array(req, lines)?)),
        None => Ok(None),
    }
}

/// `ftp_mkdir(FTP\Connection $ftp, string $directory): string|false`:
/// `MKD`, and the path the reply quotes.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["257 \"/x/d\" made", "257 made", "550 no", "250 gone", "200 ok", "202 no need", "500 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_mkdir($f, "d"), ftp_mkdir($f, "e"), ftp_mkdir($f, "f"), ftp_rmdir($f, "d"), ftp_chmod($f, 0644, "a"), ftp_alloc($f, 10, $m), $m, ftp_alloc($f, 0), ftp_alloc($f, 5, $m), $m);
/// ```
/// ```expect process
/// Warning: ftp_mkdir(): no in %s on line 5
/// string(4) "/x/d"
/// string(1) "e"
/// bool(false)
/// bool(true)
/// int(420)
/// bool(true)
/// string(7) "no need"
/// bool(false)
/// bool(false)
/// string(2) "no"
/// ```
#[php_function]
fn ftp_mkdir(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    text_or_false(req, ftp, |req, session| {
        session.mkdir(req, directory.as_bytes())
    })
}

/// `ftp_rmdir(FTP\Connection $ftp, string $directory): bool`: `RMD`,
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["250 gone", "550 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_rmdir($f, "a"), ftp_rmdir($f, "b"));
/// ```
/// ```expect process
/// Warning: ftp_rmdir(): no in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_rmdir(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Rmdir, directory.as_bytes())
    })
}

/// `ftp_chmod(FTP\Connection $ftp, int $permissions, string $filename):`
/// `int|false`: `SITE CHMOD`.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["200 ok", "500 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_chmod($f, 0755, "a"), ftp_chmod($f, -1, "b"));
/// try { ftp_chmod($f, 1, "a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// Warning: ftp_chmod(): no in %s on line 5
/// int(493)
/// bool(false)
/// ftp_chmod(): Argument #3 ($filename) must not contain any null bytes
/// ```
#[php_function]
fn ftp_chmod(
    req: &mut Request,
    ftp: Instance<Connection>,
    permissions: i64,
    filename: Path,
) -> Result<OrFalse<i64>, Thrown> {
    let done = simple(req, ftp, |req, session| {
        session.chmod(req, permissions, filename.0.as_bytes())
    })?;
    Ok(if done {
        OrFalse::Value(permissions)
    } else {
        OrFalse::False
    })
}

/// `ftp_alloc(FTP\Connection $ftp, int $size, &$response = null): bool`:
/// `ALLO`, the reply's text given back where it was read.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["202 fine", "500 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_alloc($f, 10, $m), $m, ftp_alloc($f, -1, $n), $n, ftp_alloc($f, 5));
/// ```
/// ```expect process
/// bool(true)
/// string(4) "fine"
/// bool(false)
/// NULL
/// bool(false)
/// ```
#[php_function]
fn ftp_alloc(
    req: &mut Request,
    ftp: Instance<Connection>,
    size: i64,
    #[php(default = null)] response: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let answer = lend(req, ftp, |req, session| session.alloc(req, size))?;
    let Some((done, text)) = answer else {
        return Ok(false);
    };
    if let Some(text) = text
        && response.is_given()
    {
        let text = PhpString::new(req, &text)?;
        response.set(req, Untyped(text.into_value()))?;
    }
    Ok(done)
}

/// A listing native: the lines, or `None`.
fn listing(
    req: &mut Request,
    ftp: Instance<Connection>,
    cmd: &[u8],
    path: &[u8],
) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
    Ok(lend(req, ftp, |req, session| session.genlist(req, cmd, path))?.flatten())
}

/// `ftp_nlist(FTP\Connection $ftp, string $directory): array|false`:
/// `NLST`'s lines, each ended by `\r\n`.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["NLST", "LIST", "MLSD"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=file;size=3; a\r\nbad\r\n" : "a\r\nb\r\nc"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     fwrite($c, "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_pasv($f, true), ftp_nlist($f, "."), ftp_rawlist($f, ".", true), ftp_mlsd($f, "."));
/// ```
/// ```expect process
/// Warning: ftp_mlsd(): Missing pathname in MLSD response in %s on line 9
/// bool(true)
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(1) {
///   [0]=>
///   array(3) {
///     ["name"]=>
///     string(1) "a"
///     ["type"]=>
///     string(4) "file"
///     ["size"]=>
///     string(1) "3"
///   }
/// }
/// ```
#[php_function]
fn ftp_nlist(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: Path,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    match listing(req, ftp, b"NLST", directory.0.as_bytes())? {
        Some(lines) => Ok(OrFalse::Value(lines_array(req, lines)?)),
        None => Ok(OrFalse::False),
    }
}

/// `ftp_rawlist(FTP\Connection $ftp, string $directory, bool $recursive =`
/// `false): array|false`: `LIST`'s (or `LIST -R`'s) lines.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// var_dump(ftp_rawlist($f, "."), ftp_rawlist($f, ".", true));
/// ```
/// ```expect process
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// ```
#[php_function]
fn ftp_rawlist(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: PhpString,
    #[php(default = false)] recursive: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let cmd: &[u8] = if recursive { b"LIST -R" } else { b"LIST" };
    match listing(req, ftp, cmd, directory.as_bytes())? {
        Some(lines) => Ok(OrFalse::Value(lines_array(req, lines)?)),
        None => Ok(OrFalse::False),
    }
}

/// `ftp_mlsd(FTP\Connection $ftp, string $directory): array|false`:
/// `MLSD`'s lines, each parsed into its name and facts, a line that
/// breaks the form dropped after its warning.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// var_dump(ftp_mlsd($f, "."));
/// ```
/// ```expect process
/// Warning: ftp_mlsd(): Missing pathname in MLSD response in %s on line 11
/// array(1) {
///   [0]=>
///   array(3) {
///     ["name"]=>
///     string(1) "d"
///     ["type"]=>
///     string(3) "dir"
///     ["perm"]=>
///     string(2) "el"
///   }
/// }
/// ```
#[php_function]
fn ftp_mlsd(
    req: &mut Request,
    ftp: Instance<Connection>,
    directory: PhpString,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(lines) = listing(req, ftp, b"MLSD", directory.as_bytes())? else {
        return Ok(OrFalse::False);
    };
    let mut array = PhpArray::new(req)?;
    for line in lines {
        let entry = match ftp::mlsd_line(req, &line) {
            Ok(entry) => entry,
            Err(thrown) => {
                array.give_up(req);
                return Err(thrown);
            }
        };
        if let Some(entry) = entry {
            array.append(req, entry.into_value())?;
        }
    }
    Ok(OrFalse::Value(array))
}

/// `ftp_systype(FTP\Connection $ftp): string|false`: the first word of
/// `SYST`'s reply, cached.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); fgets($c); fwrite($c, "215   UNIX Type: L8\r\n"); fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_systype($f), ftp_systype($f));
/// ```
/// ```expect process
/// string(4) "UNIX"
/// string(4) "UNIX"
/// ```
#[php_function]
fn ftp_systype(
    req: &mut Request,
    ftp: Instance<Connection>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    text_or_false(req, ftp, |req, session| session.syst(req))
}

/// The offset a transfer resumes at, `FTP_AUTORESUME` ignored where
/// autoseek is off, and whether autoseek is on.
fn resume_at(
    req: &mut Request,
    ftp: Instance<Connection>,
    offset: i64,
) -> Result<(bool, i64), Thrown> {
    let autoseek = autoseek(req, ftp)?;
    if !autoseek && offset == AUTORESUME {
        return Ok((false, 0));
    }
    Ok((autoseek, offset))
}

/// A receiving stream moved to where a download resumes: its end for
/// `FTP_AUTORESUME`, the offset otherwise.
fn seek_out(req: &mut Request, stream: Stream, autoseek: bool, offset: i64) -> i64 {
    if !autoseek || offset == 0 {
        return offset;
    }
    if offset == AUTORESUME {
        stream.seek(req, 0, Whence::End);
        return stream.tell(req);
    }
    stream.seek(req, offset, Whence::Set);
    offset
}

/// `ftp_fget(FTP\Connection $ftp, $stream, string $remote_filename, int`
/// `$mode = FTP_BINARY, int $offset = 0): bool`: `RETR` into the stream,
/// every `\r` dropped in ASCII.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if ($v == "RETR") { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, "a\r\nb\rc"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 2\r\n"); continue; }
///     if ($v == "MDTM") { fwrite($c, "213 20240102030405\r\n"); continue; }
///     fwrite($c, "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $m = fopen("php://memory", "w+");
/// var_dump(ftp_fget($f, $m, "x", FTP_ASCII), stream_get_contents($m, -1, 0));
/// ftruncate($m, 0); rewind($m);
/// var_dump(ftp_fget($f, $m, "x"), bin2hex(stream_get_contents($m, -1, 0)));
/// try { ftp_fget($f, $m, "x", 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// $t = tempnam(sys_get_temp_dir(), "ftp"); file_put_contents($t, "l1\nl2\n");
/// var_dump(ftp_put($f, "r", $t, FTP_ASCII), ftp_append($f, "r", $t), ftp_fput($f, "r", fopen($t, "r")), ftp_size($f, "r"), ftp_mdtm($f, "r"));
/// var_dump(ftp_get($f, $t, "x"), bin2hex(file_get_contents($t))); unlink($t);
/// ```
/// ```expect process
/// bool(true)
/// string(4) "a
/// bc"
/// bool(true)
/// string(12) "610d0a620d63"
/// ftp_fget(): Argument #4 ($mode) must be either FTP_ASCII or FTP_BINARY
/// bool(true)
/// bool(true)
/// bool(true)
/// int(2)
/// int(1704164645)
/// bool(true)
/// string(12) "610d0a620d63"
/// ```
#[php_function]
fn ftp_fget(
    req: &mut Request,
    ftp: Instance<Connection>,
    stream: Stream,
    remote_filename: PhpString,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<bool, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let (autoseek, offset) = resume_at(req, ftp, offset)?;
    let offset = seek_out(req, stream, autoseek, offset);
    simple(req, ftp, |req, session| {
        session.get(req, stream, remote_filename.as_bytes(), kind, offset)
    })
}

/// `ftp_nb_fget(FTP\Connection $ftp, $stream, string $remote_filename,`
/// `int $mode = FTP_BINARY, int $offset = 0): int`: `RETR` begun into the
/// stream, `ftp_nb_continue` reading the rest, a `\r` kept in ASCII
/// where no `\n` follows it.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if ($v == "RETR") { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, "a\r\nb\rc\r"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if ($v == "STOR") { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); while (!feof($x)) fread($x, 99); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     fwrite($c, "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// var_dump(ftp_nb_continue($f));
/// $m = fopen("php://memory", "w+");
/// for ($r = ftp_nb_fget($f, $m, "x", FTP_ASCII); $r == FTP_MOREDATA; $r = @ftp_nb_continue($f));
/// var_dump($r, bin2hex(stream_get_contents($m, -1, 0)));
/// $t = tempnam(sys_get_temp_dir(), "ftp");
/// for ($r = ftp_nb_get($f, $t, "x"); $r == FTP_MOREDATA; $r = @ftp_nb_continue($f));
/// var_dump($r, bin2hex(file_get_contents($t)));
/// for ($r = ftp_nb_put($f, "y", $t); $r == FTP_MOREDATA; $r = @ftp_nb_continue($f));
/// for ($q = ftp_nb_fput($f, "y", fopen($t, "r"), FTP_ASCII); $q == FTP_MOREDATA; $q = @ftp_nb_continue($f));
/// var_dump($r, $q); unlink($t);
/// ```
/// ```expect process
/// Warning: ftp_nb_continue(): No non-blocking transfer to continue in %s on line 10
/// int(0)
/// int(1)
/// string(12) "610a620d630d"
/// int(1)
/// string(14) "610d0a620d630d"
/// int(1)
/// int(1)
/// ```
#[php_function]
fn ftp_nb_fget(
    req: &mut Request,
    ftp: Instance<Connection>,
    stream: Stream,
    remote_filename: PhpString,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<i64, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let (autoseek, offset) = resume_at(req, ftp, offset)?;
    let offset = seek_out(req, stream, autoseek, offset);
    let mut held = Some(Owner::take(req, stream));
    let progress = lend(req, ftp, |req, session| {
        let Some(stream) = held.take() else {
            return Ok(Progress::Failed);
        };
        let progress =
            session.nb_get(req, stream, false, remote_filename.as_bytes(), kind, offset)?;
        if progress == Progress::Failed {
            warn_reply(req, session);
        }
        Ok(progress)
    });
    if let Some(stream) = held {
        stream.give_up(req);
    }
    Ok(progress?.unwrap_or(Progress::Failed) as i64)
}

/// `ftp_pasv(FTP\Connection $ftp, bool $enable): bool`: passive transfers, the
/// address asked for at once.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// var_dump(ftp_pasv($f, false), ftp_pasv($f, true), ftp_nlist($f, "."));
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
/// }
/// ```
#[php_function]
fn ftp_pasv(req: &mut Request, ftp: Instance<Connection>, enable: bool) -> Result<bool, Thrown> {
    Ok(lend(req, ftp, |req, session| session.pasv(req, enable))?.unwrap_or(false))
}

/// The local file a download writes: `rb+` (or `rt+`) where it resumes,
/// `wb` (or `wt`) where that fails or it does not; the offset moved to.
fn local_out(
    req: &mut Request,
    path: &[u8],
    kind: Kind,
    autoseek: bool,
    offset: i64,
) -> Result<Option<(Owner<Stream>, i64)>, Thrown> {
    let (update, write): (&[u8], &[u8]) = match kind {
        Kind::Ascii => (b"rt+", b"wt"),
        Kind::Image => (b"rb+", b"wb"),
    };
    let stream = if autoseek && offset != 0 {
        match req.open_stream(path, update, false)? {
            Some(stream) => Some(stream),
            None => req.open_stream(path, write, false)?,
        }
    } else {
        req.open_stream(path, write, false)?
    };
    let Some(stream) = stream else {
        let text = format!("Error opening {}", String::from_utf8_lossy(c_str(path)));
        docref(req, Level::Warning, &text);
        return Ok(None);
    };
    let offset = seek_out(req, *stream, autoseek, offset);
    Ok(Some((stream, offset)))
}

/// `VCWD_UNLINK(local)` after a download failed.
fn unlink(req: &mut Request, path: &[u8]) {
    let path = String::from_utf8_lossy(c_str(path)).into_owned();
    std::fs::remove_file(req.environment().cwd().join(path)).ok();
}

/// `ftp_get(FTP\Connection $ftp, string $local_filename, string`
/// `$remote_filename, int $mode = FTP_BINARY, int $offset = 0): bool`:
/// `RETR` into the local file, which a failure removes.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $t = tempnam(sys_get_temp_dir(), "ftp");
/// var_dump(ftp_get($f, $t, "a", FTP_ASCII), file_get_contents($t)); unlink($t);
/// ```
/// ```expect process
/// bool(true)
/// string(4) "a
/// b
/// "
/// ```
#[php_function]
fn ftp_get(
    req: &mut Request,
    ftp: Instance<Connection>,
    local_filename: Path,
    remote_filename: Path,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<bool, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let (autoseek, offset) = resume_at(req, ftp, offset)?;
    let local = local_filename.0.as_bytes();
    let Some((out, offset)) = local_out(req, local, kind, autoseek, offset)? else {
        return Ok(false);
    };
    let mut held = Some(out);
    let got = lend(req, ftp, |req, session| {
        let Some(out) = held.take() else {
            return Ok(false);
        };
        let got = session.get(req, *out, remote_filename.0.as_bytes(), kind, offset);
        out.free(req);
        if !got? {
            unlink(req, local);
            warn_reply(req, session);
            return Ok(false);
        }
        Ok(true)
    });
    if let Some(out) = held {
        out.free(req);
    }
    Ok(got? == Some(true))
}

/// `ftp_nb_get(FTP\Connection $ftp, string $local_filename, string`
/// `$remote_filename, int $mode = FTP_BINARY, int $offset = 0):`
/// `int|false`: `RETR` begun into the local file, closed when the
/// transfer ends.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $t = tempnam(sys_get_temp_dir(), "ftp");
/// for ($r = ftp_nb_get($f, $t, "a"); $r == FTP_MOREDATA; $r = ftp_nb_continue($f));
/// var_dump($r, bin2hex(file_get_contents($t))); unlink($t);
/// ```
/// ```expect process
/// int(1)
/// string(12) "610d0a620d0a"
/// ```
#[php_function]
fn ftp_nb_get(
    req: &mut Request,
    ftp: Instance<Connection>,
    local_filename: PhpString,
    remote_filename: PhpString,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    open(req, ftp)?;
    if matches!(**ftp.payload_mut(req)?, Conn::Lent) {
        docref(req, Level::Warning, "FTP\\Connection is already in use");
        return Ok(OrFalse::False);
    }
    let kind = xtype(req, mode)?;
    let (autoseek, offset) = resume_at(req, ftp, offset)?;
    let local = local_filename.as_bytes();
    let Some((out, offset)) = local_out(req, local, kind, autoseek, offset)? else {
        return Ok(OrFalse::False);
    };
    let mut held = Some(out);
    let progress = lend(req, ftp, |req, session| {
        let Some(out) = held.take() else {
            return Ok(Progress::Failed);
        };
        let progress = session.nb_get(req, out, true, remote_filename.as_bytes(), kind, offset)?;
        if progress == Progress::Failed {
            unlink(req, local);
            warn_reply(req, session);
        }
        Ok(progress)
    });
    if let Some(out) = held {
        out.free(req);
    }
    let Some(progress) = progress? else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(progress as i64))
}

/// `ftp_nb_continue(FTP\Connection $ftp): int`: one more read or write
/// of the transfer in flight.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// var_dump(@ftp_nb_continue($f));
/// $m = fopen("php://memory", "w+");
/// for ($r = ftp_nb_fget($f, $m, "a"); $r == FTP_MOREDATA; $r = ftp_nb_continue($f));
/// var_dump($r, bin2hex(stream_get_contents($m, -1, 0)));
/// ```
/// ```expect process
/// int(0)
/// int(1)
/// string(12) "610d0a620d0a"
/// ```
#[php_function]
fn ftp_nb_continue(req: &mut Request, ftp: Instance<Connection>) -> Result<i64, Thrown> {
    let progress = lend(req, ftp, |req, session| {
        if session.nb.is_none() {
            docref(req, Level::Warning, "No non-blocking transfer to continue");
            return Ok(Progress::Failed);
        }
        let progress = session.nb_continue(req)?;
        if progress == Progress::Failed {
            let text = String::from_utf8_lossy(c_str(&session.inbuf)).into_owned();
            docref(req, Level::Warning, &text);
        }
        Ok(progress)
    })?;
    Ok(progress.unwrap_or(Progress::Failed) as i64)
}

/// The offset an upload resumes at, `FTP_AUTORESUME` asking the server
/// for the remote size, and the stream moved to it.
fn seek_in(
    req: &mut Request,
    ftp: Instance<Connection>,
    stream: Stream,
    remote: &[u8],
    offset: i64,
) -> Result<i64, Thrown> {
    let (autoseek, mut offset) = resume_at(req, ftp, offset)?;
    if !autoseek || offset == 0 {
        return Ok(offset);
    }
    if offset == AUTORESUME {
        offset = lend(req, ftp, |req, session| session.size(req, remote))?
            .unwrap_or(-1)
            .max(0);
    }
    if offset != 0 {
        stream.seek(req, offset, Whence::Set);
    }
    Ok(offset)
}

/// `ftp_fput(FTP\Connection $ftp, string $remote_filename, $stream, int`
/// `$mode = FTP_BINARY, int $offset = 0): bool`: `STOR` of the stream;
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $m = fopen("php://memory", "w+"); fwrite($m, "x\ny"); rewind($m);
/// var_dump(ftp_fput($f, "a", $m, FTP_ASCII), ftp_raw($f, "NOOP"));
/// ```
/// ```expect process
/// bool(true)
/// array(1) {
///   [0]=>
///   string(6) "200 ok"
/// }
/// ```
#[php_function]
fn ftp_fput(
    req: &mut Request,
    ftp: Instance<Connection>,
    remote_filename: PhpString,
    stream: Stream,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<bool, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let remote = remote_filename.as_bytes();
    let offset = seek_in(req, ftp, stream, remote, offset)?;
    simple(req, ftp, |req, session| {
        session.put(req, b"STOR", remote, stream, kind, offset)
    })
}

/// `ftp_nb_fput(FTP\Connection $ftp, string $remote_filename, $stream,`
/// `int $mode = FTP_BINARY, int $offset = 0): int`: `STOR` begun from the
/// stream.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $m = fopen("php://memory", "w+"); fwrite($m, "xyz"); rewind($m);
/// for ($r = ftp_nb_fput($f, "a", $m); $r == FTP_MOREDATA; $r = ftp_nb_continue($f));
/// var_dump($r, ftell($m));
/// ```
/// ```expect process
/// int(1)
/// int(3)
/// ```
#[php_function]
fn ftp_nb_fput(
    req: &mut Request,
    ftp: Instance<Connection>,
    remote_filename: PhpString,
    stream: Stream,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<i64, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let remote = remote_filename.as_bytes();
    let offset = seek_in(req, ftp, stream, remote, offset)?;
    let mut held = Some(Owner::take(req, stream));
    let progress = lend(req, ftp, |req, session| {
        let Some(stream) = held.take() else {
            return Ok(Progress::Failed);
        };
        let progress = session.nb_put(req, remote, stream, false, kind, offset)?;
        if progress == Progress::Failed {
            warn_reply(req, session);
        }
        Ok(progress)
    });
    if let Some(stream) = held {
        stream.give_up(req);
    }
    Ok(progress?.unwrap_or(Progress::Failed) as i64)
}

/// The local file an upload reads, `rb` or `rt`.
fn local_in(req: &mut Request, path: &[u8], kind: Kind) -> Result<Option<Owner<Stream>>, Thrown> {
    let mode: &[u8] = match kind {
        Kind::Ascii => b"rt",
        Kind::Image => b"rb",
    };
    req.open_stream(path, mode, false)
}

/// `ftp_put(FTP\Connection $ftp, string $remote_filename, string`
/// `$local_filename, int $mode = FTP_BINARY, int $offset = 0): bool`:
/// `STOR` of the local file.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $t = tempnam(sys_get_temp_dir(), "ftp"); file_put_contents($t, "abc");
/// var_dump(ftp_put($f, "a", $t), @ftp_put($f, "a", "/nonexistent/x")); unlink($t);
/// ```
/// ```expect process
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_put(
    req: &mut Request,
    ftp: Instance<Connection>,
    remote_filename: Path,
    local_filename: Path,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<bool, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let Some(input) = local_in(req, local_filename.0.as_bytes(), kind)? else {
        return Ok(false);
    };
    let remote = remote_filename.0.as_bytes();
    let view = *input;
    let put = seek_in(req, ftp, view, remote, offset).and_then(|offset| {
        simple(req, ftp, |req, session| {
            session.put(req, b"STOR", remote, view, kind, offset)
        })
    });
    input.free(req);
    put
}

/// `ftp_append(FTP\Connection $ftp, string $remote_filename, string`
/// `$local_filename, int $mode = FTP_BINARY): bool`: `APPE` of the local
/// file.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $t = tempnam(sys_get_temp_dir(), "ftp"); file_put_contents($t, "abc");
/// var_dump(ftp_append($f, "a", $t, FTP_ASCII)); unlink($t);
/// ```
/// ```expect process
/// bool(true)
/// ```
#[php_function]
fn ftp_append(
    req: &mut Request,
    ftp: Instance<Connection>,
    remote_filename: Path,
    local_filename: Path,
    #[php(default = FTP_BINARY)] mode: i64,
) -> Result<bool, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let Some(input) = local_in(req, local_filename.0.as_bytes(), kind)? else {
        return Ok(false);
    };
    let remote = remote_filename.0.as_bytes();
    let view = *input;
    let put = simple(req, ftp, |req, session| {
        session.put(req, b"APPE", remote, view, kind, 0)
    });
    input.free(req);
    put
}

/// `ftp_nb_put(FTP\Connection $ftp, string $remote_filename, string`
/// `$local_filename, int $mode = FTP_BINARY, int $offset = 0):`
/// `int|false`: `STOR` begun from the local file, closed when the
/// transfer ends.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); $d = stream_socket_server("tcp://127.0.0.1:0"); $q = (int) explode(":", stream_socket_get_name($d, false))[1];
///   while ($l = fgets($c)) { $v = strtok(trim($l), " ");
///     if ($v == "PASV") { fwrite($c, "227 (127,0,0,1," . ($q >> 8) . "," . ($q & 255) . ")\r\n"); continue; }
///     if (in_array($v, ["RETR", "LIST", "MLSD", "NLST"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); fwrite($x, $v == "MLSD" ? "type=dir;perm=el; d\r\nx;\r\n" : "a\r\nb\r\n"); fclose($x); fwrite($c, "226 done\r\n"); continue; }
///     if (in_array($v, ["STOR", "APPE"])) { fwrite($c, "150 go\r\n"); $x = stream_socket_accept($d); $got = ""; while (!feof($x)) $got .= fread($x, 99); fclose($x); fwrite($c, "226 got " . bin2hex($got) . "\r\n"); continue; }
///     if ($v == "SIZE") { fwrite($c, "213 1\r\n"); continue; }
///     fwrite($c, $v == "DELE" ? "550 no\r\n" : "200 ok\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p); ftp_pasv($f, true);
/// $t = tempnam(sys_get_temp_dir(), "ftp"); file_put_contents($t, "abc");
/// for ($r = ftp_nb_put($f, "a", $t); $r == FTP_MOREDATA; $r = ftp_nb_continue($f));
/// var_dump($r); unlink($t);
/// ```
/// ```expect process
/// int(1)
/// ```
#[php_function]
fn ftp_nb_put(
    req: &mut Request,
    ftp: Instance<Connection>,
    remote_filename: Path,
    local_filename: Path,
    #[php(default = FTP_BINARY)] mode: i64,
    #[php(default = 0)] offset: i64,
) -> Result<OrFalse<i64>, Thrown> {
    open(req, ftp)?;
    let kind = xtype(req, mode)?;
    let Some(input) = local_in(req, local_filename.0.as_bytes(), kind)? else {
        return Ok(OrFalse::False);
    };
    let remote = remote_filename.0.as_bytes();
    let offset = match seek_in(req, ftp, *input, remote, offset) {
        Ok(offset) => offset,
        Err(thrown) => {
            input.free(req);
            return Err(thrown);
        }
    };
    let mut held = Some(input);
    let progress = lend(req, ftp, |req, session| {
        let Some(input) = held.take() else {
            return Ok(Progress::Failed);
        };
        let progress = session.nb_put(req, remote, input, true, kind, offset)?;
        if progress == Progress::Failed {
            let text = String::from_utf8_lossy(c_str(&session.inbuf)).into_owned();
            docref(req, Level::Warning, &text);
        }
        Ok(progress)
    });
    if let Some(input) = held {
        input.free(req);
    }
    let Some(progress) = progress? else {
        return Ok(OrFalse::False);
    };
    Ok(OrFalse::Value(progress as i64))
}

/// `ftp_size(FTP\Connection $ftp, string $filename): int`: `SIZE`'s
/// number, `-1` where it fails.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["200 binary", "213 12345", "550 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_size($f, "a"), ftp_size($f, "b"));
/// ```
/// ```expect process
/// int(12345)
/// int(-1)
/// ```
#[php_function]
fn ftp_size(req: &mut Request, ftp: Instance<Connection>, filename: Path) -> Result<i64, Thrown> {
    Ok(lend(req, ftp, |req, session| {
        session.size(req, filename.0.as_bytes())
    })?
    .unwrap_or(-1))
}

/// `ftp_mdtm(FTP\Connection $ftp, string $filename): int`: `MDTM`'s time,
/// `-1` where it fails.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["213 20000101000000", "213 bad", "550 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_mdtm($f, "a"), ftp_mdtm($f, "b"), ftp_mdtm($f, "c"));
/// ```
/// ```expect process
/// int(946684800)
/// int(-1)
/// int(-1)
/// ```
#[php_function]
fn ftp_mdtm(req: &mut Request, ftp: Instance<Connection>, filename: Path) -> Result<i64, Thrown> {
    Ok(lend(req, ftp, |req, session| {
        session.mdtm(req, filename.0.as_bytes())
    })?
    .unwrap_or(-1))
}

/// `ftp_rename(FTP\Connection $ftp, string $from, string $to): bool`:
/// `RNFR` and `RNTO`.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["350 go on", "250 moved", "550 not there", "250 deleted", "214 site ok", "500 no", "221 bye"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_rename($f, "a", "b"), ftp_rename($f, "c", "d"), ftp_delete($f, "b"), ftp_site($f, "X"), ftp_site($f, "Y"), ftp_quit($f));
/// try { ftp_pwd($f); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(ftp_close($f));
/// ```
/// ```expect process
/// Warning: ftp_rename(): not there in %s on line 5
///
/// Warning: ftp_site(): no in %s on line 5
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// FTP\Connection is already closed
/// bool(true)
/// ```
#[php_function]
fn ftp_rename(
    req: &mut Request,
    ftp: Instance<Connection>,
    from: PhpString,
    to: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.rename(req, from.as_bytes(), to.as_bytes())
    })
}

/// `ftp_delete(FTP\Connection $ftp, string $filename): bool`: `DELE`,
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["250 gone", "550 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_delete($f, "a"), ftp_delete($f, "b"));
/// ```
/// ```expect process
/// Warning: ftp_delete(): no in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_delete(
    req: &mut Request,
    ftp: Instance<Connection>,
    filename: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Delete, filename.as_bytes())
    })
}

/// `ftp_site(FTP\Connection $ftp, string $command): bool`: `SITE`, any
/// completion taken.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["214 ok", "500 no"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_site($f, "A"), ftp_site($f, "B"));
/// ```
/// ```expect process
/// Warning: ftp_site(): no in %s on line 5
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ftp_site(
    req: &mut Request,
    ftp: Instance<Connection>,
    command: PhpString,
) -> Result<bool, Thrown> {
    simple(req, ftp, |req, session| {
        session.simple(req, Verb::Site, command.as_bytes())
    })
}

/// `ftp_close(FTP\Connection $ftp): bool`, and `ftp_quit`, its alias:
/// `QUIT`, then the session closed; `true` for one closed already, and
/// php's `Error` for one serving a transfer.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach (["221 bye"] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_close($f), ftp_close($f));
/// try { ftp_pwd($f); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// bool(true)
/// bool(true)
/// FTP\Connection is already closed
/// ```
#[php_function(aliases(ftp_quit))]
fn ftp_close(req: &mut Request, ftp: Instance<Connection>) -> Result<bool, Thrown> {
    let conn = std::mem::take(&mut **ftp.payload_mut(req)?);
    match conn {
        Conn::Closed => Ok(true),
        Conn::Lent => {
            **ftp.payload_mut(req)? = Conn::Lent;
            let ce = Throwables::of(req)?;
            Err(throw_error(
                req,
                ce.error,
                "Cannot close FTP\\Connection while a transfer is in progress",
            ))
        }
        Conn::Open(mut session) => {
            **ftp.payload_mut(req)? = Conn::Lent;
            let quit = session.quit(req);
            session.close(req);
            **ftp.payload_mut(req)? = Conn::Closed;
            quit
        }
    }
}

/// The options of `ftp_set_option` and `ftp_get_option`.
#[derive(Clone, Copy)]
enum Opt {
    Timeout,
    Autoseek,
    UsePasvAddress,
}

impl Opt {
    /// The option `$option` names, or the `ValueError` for another.
    fn of(req: &mut Request, option: i64) -> Result<Opt, Thrown> {
        match option {
            0 => Ok(Opt::Timeout),
            1 => Ok(Opt::Autoseek),
            2 => Ok(Opt::UsePasvAddress),
            _ => Err(arg_value_error(
                req,
                2,
                "be one of FTP_TIMEOUT_SEC, FTP_AUTOSEEK, or FTP_USEPASVADDRESS",
            )),
        }
    }
}

/// `ftp_set_option(FTP\Connection $ftp, int $option, $value): true`: the
/// timeout (an `int` above zero), autoseek or whether the passive
/// address is used (each a `bool`).
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_get_option($f, FTP_TIMEOUT_SEC), ftp_get_option($f, FTP_AUTOSEEK), ftp_get_option($f, FTP_USEPASVADDRESS));
/// var_dump(ftp_set_option($f, FTP_TIMEOUT_SEC, 5), ftp_set_option($f, FTP_AUTOSEEK, false), ftp_get_option($f, FTP_TIMEOUT_SEC), ftp_get_option($f, FTP_AUTOSEEK));
/// foreach ([[FTP_TIMEOUT_SEC, "5"], [FTP_TIMEOUT_SEC, 0], [FTP_TIMEOUT_SEC, PHP_INT_MAX], [FTP_AUTOSEEK, 1], [FTP_USEPASVADDRESS, null], [9, 1]] as [$o, $v]) { try { ftp_set_option($f, $o, $v); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
/// try { ftp_get_option($f, 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect process
/// int(90)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// int(5)
/// bool(false)
/// ftp_set_option(): Argument #3 ($value) must be of type int for the FTP_TIMEOUT_SEC option, string given
/// ftp_set_option(): Argument #3 ($value) must be greater than 0 for the FTP_TIMEOUT_SEC option
/// ftp_set_option(): Argument #3 ($value) must be less than 18446744073709 for the FTP_TIMEOUT_SEC option
/// ftp_set_option(): Argument #3 ($value) must be of type bool for the FTP_AUTOSEEK option, int given
/// ftp_set_option(): Argument #3 ($value) must be of type bool for the FTP_USEPASVADDRESS option, null given
/// ftp_set_option(): Argument #2 ($option) must be one of FTP_TIMEOUT_SEC, FTP_AUTOSEEK, or FTP_USEPASVADDRESS
/// ftp_get_option(): Argument #2 ($option) must be one of FTP_TIMEOUT_SEC, FTP_AUTOSEEK, or FTP_USEPASVADDRESS
/// ```
#[php_function]
fn ftp_set_option(
    req: &mut Request,
    ftp: Instance<Connection>,
    option: i64,
    value: Untyped<Value>,
) -> Result<True, Thrown> {
    open(req, ftp)?;
    let value = value.0;
    let opt = Opt::of(req, option)?;
    let wrong = |req: &mut Request, ty: &str, name: &str| {
        let given = value_name(req, &value);
        let tail = format!("be of type {ty} for the {name} option, ");
        arg_type_error(req, 3, &[tail.as_bytes(), &given, b" given"].concat())
    };
    let setting = match (opt, value.as_ref()) {
        (Opt::Timeout, ValueRef::Int(n)) => {
            bounded_timeout(req, n, TimeoutFrom::SetOption)?;
            Setting::Timeout(n)
        }
        (Opt::Timeout, _) => return Err(wrong(req, "int", "FTP_TIMEOUT_SEC")),
        (Opt::Autoseek, ValueRef::Bool(b)) => Setting::Autoseek(b),
        (Opt::Autoseek, _) => return Err(wrong(req, "bool", "FTP_AUTOSEEK")),
        (Opt::UsePasvAddress, ValueRef::Bool(b)) => Setting::UsePasvAddress(b),
        (Opt::UsePasvAddress, _) => return Err(wrong(req, "bool", "FTP_USEPASVADDRESS")),
    };
    if let Conn::Open(session) = &mut **ftp.payload_mut(req)? {
        match setting {
            Setting::Timeout(n) => session.timeout = n,
            Setting::Autoseek(b) => session.autoseek = b,
            Setting::UsePasvAddress(b) => session.usepasvaddress = b,
        }
    }
    Ok(True)
}

/// One option's new value.
enum Setting {
    Timeout(i64),
    Autoseek(bool),
    UsePasvAddress(bool),
}

/// `ftp_get_option(FTP\Connection $ftp, int $option): int|bool`;
///.
///
/// ```php
/// $s = stream_socket_server("tcp://127.0.0.1:0"); $p = (int) explode(":", stream_socket_get_name($s, false))[1];
/// if (!pcntl_fork()) { $c = stream_socket_accept($s); fwrite($c, "220 hi\r\n"); foreach ([] as $r) { fgets($c); fwrite($c, "$r\r\n"); } fgets($c); exit; }
/// $f = ftp_connect("127.0.0.1", $p);
/// var_dump(ftp_get_option($f, FTP_TIMEOUT_SEC), ftp_get_option($f, FTP_USEPASVADDRESS));
/// ```
/// ```expect process
/// int(90)
/// bool(true)
/// ```
#[php_function]
fn ftp_get_option(
    req: &mut Request,
    ftp: Instance<Connection>,
    option: i64,
) -> Result<Union<i64, bool>, Thrown> {
    open(req, ftp)?;
    let opt = Opt::of(req, option)?;
    let Conn::Open(session) = &**ftp.payload_mut(req)? else {
        return Ok(Union::B(false));
    };
    Ok(match opt {
        Opt::Timeout => Union::A(session.timeout),
        Opt::Autoseek => Union::B(session.autoseek),
        Opt::UsePasvAddress => Union::B(session.usepasvaddress),
    })
}

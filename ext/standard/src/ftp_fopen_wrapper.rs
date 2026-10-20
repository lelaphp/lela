//! `Z/ext/standard/ftp_fopen_wrapper.c`: the `ftp://` wrapper over the
//! engine's `tcp` socket `transport`, php's control connection logged in
//! once per operation and its data connection made passively. A stream
//! the wrapper opens is the data connection, with the control one kept
//! beside it for the close ([`Control`]); a directory is the `NLST` of
//! one, read whole; `stat()`, `unlink()`, `rename()`, `mkdir()` and
//! `rmdir()` are one exchange each. Each reply of the control connection
//! is a [`Reply`], its code's class matched as RFC 959 sorts it where php
//! compares ranges. `ftps://` is the same wrapper with its connections
//! secured through `ext/openssl`'s crypto layer, which registers it again
//! under `ftps`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::c_str;
use php_ext::coerce::{atoi, is_space, strtol, truthy};
use php_ext::raising::{Level, Thrown, docref, throw_error};
use php_ext::request::{
    Closer, DirListing, Handshake, MAXPATHLEN, Notification, NotifyCode, Opening, OwnedStream as _,
    Request, SocketOptions, Stream, StreamCtx, StreamWrapper, Streams as _, Xport, XportFailure,
};
use php_ext::values::{Owner, PhpArray, ValueRef};
use php_ext_uri::UriStruct;

use crate::http_fopen_wrapper::{file_ini, url_wrap_http};
use crate::sockets::{default_timeout, timeval};
use crate::streamsfuncs::{notify, notify_error, notify_info};
use crate::string::Decimal;
use crate::url::{UriParser, php_raw_url_decode};

/// `php_stream_ftp_wrapper`: `ftp`, a URL wrapper that opens streams and
/// directories, stats, unlinks, renames and makes and removes
/// directories, and whose streams no `fstat()` answers.
pub fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "ftp".into(),
        label: "ftp",
        is_url: true,
        open: Some(open),
        open_dir: Some(open_dir),
        stat: Some(url_stat),
        unlink: Some(unlink),
        rename: Some(rename),
        mkdir: Some(mkdir),
        rmdir: Some(rmdir),
        set_stat: None,
        stream_stat: Some(stream_stat),
    }
}

/// `php_stream_ftp_stream_stat`: `-1`, so the socket's own stat is not
/// taken for the file's.
fn stream_stat(_req: &mut Request, _stream: Stream) -> Option<[i64; 13]> {
    None
}

/// The class of a reply's code, RFC 959's first digit, which php's
/// range comparisons read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// `1yz`, a positive preliminary reply.
    Preliminary,
    /// `2yz`, a positive completion reply.
    Completion,
    /// `3yz`, a positive intermediate reply.
    Intermediate,
    /// Anything else: a refusal, or no reply at all.
    Refusal,
}

/// One reply of the control connection as `get_ftp_result` reads it:
/// lines read until one opens with three digits and a blank, the code
/// `strtol` makes of the last line read, and that line, with its end,
/// which php's messages print.
#[derive(Debug, Clone, Default)]
struct Reply {
    code: i64,
    line: Vec<u8>,
}

impl Reply {
    fn class(&self) -> Class {
        match self.code {
            100..=199 => Class::Preliminary,
            200..=299 => Class::Completion,
            300..=399 => Class::Intermediate,
            _ => Class::Refusal,
        }
    }
}

/// `get_ftp_result` (`Z/ext/standard/ftp_fopen_wrapper.c:79`) over a
/// 512-byte buffer: lines read until one opens `ddd `, the last read kept
/// where a read gives none.
fn get_result(req: &mut Request, stream: Stream) -> Result<Reply, Thrown> {
    let mut line = Vec::new();
    while let Some(read) = stream.get_line(req, Some(511))? {
        line = read;
        let reply = line.len() >= 4 && line[..3].iter().all(u8::is_ascii_digit) && line[3] == b' ';
        if reply {
            break;
        }
    }
    Ok(Reply {
        // `get_ftp_result` answers its `strtol` as an `int`: C's `atoi`.
        code: i64::from(atoi(c_str(&line))),
        line,
    })
}

/// `php_stream_write_string` and `php_stream_printf`: one command, one
/// write, `\r\n` after it.
fn command(req: &mut Request, stream: Stream, verb: &[u8], argument: Option<&[u8]>) {
    let mut line = verb.to_vec();
    if let Some(argument) = argument {
        line.push(b' ');
        line.extend_from_slice(c_str(argument));
    }
    line.extend_from_slice(b"\r\n");
    stream.write(req, &line);
}

/// How an operation reports a refusal: the opener's logged errors, which
/// the open shows where it fails, or nothing, as the other operations
/// hand the connect `0` for its `options`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Report {
    Log,
    Quiet,
}

/// `php_stream_wrapper_log_error` under `report`.
fn log(req: &mut Request, report: Report, message: &[u8]) {
    if report == Report::Log {
        req.wrapper_log_error(&String::from_utf8_lossy(message));
    }
}

/// A connection logged in: the control stream, the URL it was made for,
/// and, for an `ftps://` URL, whether its data connections are secured
/// too (`use_ssl_on_data`).
struct Session {
    control: Owner<Stream>,
    resource: UriStruct,
    ssl_on_data: bool,
}

/// `STREAM_CRYPTO_METHOD_SSLv23_CLIENT`, what `ftps://` secures its
/// connections with.
const SSLV23_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | 1;

/// `php_stream_xport_crypto_setup(stream, STREAM_CRYPTO_METHOD_SSLv23_CLIENT,
/// NULL) < 0 || php_stream_xport_crypto_enable(stream, 1) < 0` answered
/// no: whether the stream's crypto is on.
fn secured(req: &mut Request, stream: Stream) -> bool {
    stream.crypto_setup(req, SSLV23_CLIENT, None)
        && !matches!(
            stream.crypto_enable(req, true),
            Handshake::Failed | Handshake::Unsupported
        )
}

/// `php_ftp_fopen_connect` (`Z/ext/standard/ftp_fopen_wrapper.c:127`):
/// the URL parsed with the `context`'s parser, the control connection
/// made to its host and `port`, 21 where it names none, the greeting
/// read; for `ftps://`, `AUTH TLS`, or `AUTH SSL` where the server
/// refuses it, the control connection secured, `PBSZ 0` and `PROT P`;
/// then the user logged in, `anonymous` where none is named, with the
/// password or `from` or `anonymous` where the server asks for one.
/// `Ok(None)` is a connection that failed, after php's warning for a
/// socket that did not connect and the log of a login it refuses.
///
/// ```php
/// var_dump(@file_get_contents('ftps://127.0.0.1:1/x'), error_get_last()['message']);
/// ```
/// ```expect
/// bool(false)
/// string(80) "file_get_contents(ftps://127.0.0.1:1/x): Failed to open stream: operation failed"
/// ```
fn connect(
    req: &mut Request,
    path: &[u8],
    ctx: Option<StreamCtx>,
    report: Report,
) -> Result<Option<Session>, Thrown> {
    let Some(parser) = UriParser::of_ctx(req, ctx, b"ftp")? else {
        let text = format!(
            "{}(): Provided stream context has invalid value for the \"uri_parser_class\" option",
            req.frames().active_function_name()
        );
        return Err(throw_error(req, "ValueError", &text));
    };
    let Some(mut resource) = parser.parse(req, c_str(path))? else {
        return Ok(None);
    };
    if resource.path.is_none() {
        return Ok(None);
    }
    if resource.port.unwrap_or(0) == 0 {
        resource.port = Some(21);
    }
    let mut transport = b"tcp://".to_vec();
    transport.extend_from_slice(resource.host.as_deref().map(c_str).unwrap_or_default());
    transport.extend_from_slice(format!(":{}", resource.port.unwrap_or(21)).as_bytes());
    let timeout = timeval(default_timeout(req).trunc());
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent: None,
        socket: SocketOptions::default(),
        context: ctx,
    };
    let control = match req.xport_create(&transport, xport)? {
        Ok(stream) => stream,
        Err(failed) => {
            docref(req, Level::Warning, &failed.warning());
            return Ok(None);
        }
    };
    control.set_ctx(req, ctx);
    // `connect_errexit`: the control connection closed wherever the
    // session is not made.
    match handshake(req, *control, ctx, &resource, report) {
        Ok(Some(ssl_on_data)) => Ok(Some(Session {
            control,
            resource,
            ssl_on_data,
        })),
        Ok(None) => {
            control.free(req);
            Ok(None)
        }
        Err(thrown) => {
            control.free(req);
            Err(thrown)
        }
    }
}

/// The exchange of `php_ftp_fopen_connect` over the connected `control`:
/// the greeting, `ftps://`'s securing and the login; whether the data
/// connections are secured too, and `None` where the session failed.
fn handshake(
    req: &mut Request,
    control: Stream,
    ctx: Option<StreamCtx>,
    resource: &UriStruct,
    report: Report,
) -> Result<Option<bool>, Thrown> {
    notify_info(req, ctx, NotifyCode::Connect, None, 0)?;
    let greeting = get_result(req, control)?;
    if greeting.class() != Class::Completion {
        notify_error(
            req,
            ctx,
            NotifyCode::Failure,
            Some(&greeting.line),
            greeting.code,
        )?;
        return Ok(None);
    }
    let use_ssl = resource
        .scheme
        .as_deref()
        .is_some_and(|scheme| scheme.len() > 3 && scheme[3] == b's');
    let ssl_on_data = if use_ssl {
        let Some(on_data) = secure_control(req, control, report)? else {
            return Ok(None);
        };
        on_data
    } else {
        false
    };
    if !login(req, control, ctx, resource, report)? {
        return Ok(None);
    }
    Ok(Some(ssl_on_data))
}

/// `body` over the session's control connection, then the connection
/// closed, whatever `body` came to.
fn closing<T>(
    req: &mut Request,
    control: Owner<Stream>,
    body: impl FnOnce(&mut Request, Stream) -> Result<T, Thrown>,
) -> Result<T, Thrown> {
    let answer = body(req, *control);
    control.free(req);
    answer
}

/// The login of `php_ftp_fopen_connect`: `USER`, the user the URL names
/// or `anonymous`, and `PASS` where the server asks for one, the URL's
/// password or `from` or `anonymous`; whether the server took it, a name
/// or password with a control character refused first with php's log.
fn login(
    req: &mut Request,
    control: Stream,
    ctx: Option<StreamCtx>,
    resource: &UriStruct,
    report: Report,
) -> Result<bool, Thrown> {
    let user = resource.user.as_deref().map(php_raw_url_decode);
    if let Some(user) = &user
        && user.iter().any(u8::is_ascii_control)
    {
        let mut message = b"Invalid login ".to_vec();
        message.extend_from_slice(c_str(user));
        log(req, report, &message);
        return Ok(false);
    }
    command(
        req,
        control,
        b"USER",
        Some(user.as_deref().unwrap_or(b"anonymous")),
    );
    let mut reply = get_result(req, control)?;
    if reply.class() == Class::Intermediate {
        notify_info(req, ctx, NotifyCode::AuthRequired, Some(&reply.line), 0)?;
        let password = match &resource.password {
            Some(password) => {
                let password = php_raw_url_decode(password);
                if password.iter().any(u8::is_ascii_control) {
                    let mut message = b"Invalid password ".to_vec();
                    message.extend_from_slice(c_str(&password));
                    log(req, report, &message);
                    return Ok(false);
                }
                password
            }
            None => file_ini(req, b"from").unwrap_or_else(|| b"anonymous".to_vec()),
        };
        command(req, control, b"PASS", Some(&password));
        reply = get_result(req, control)?;
        if reply.class() == Class::Completion {
            notify_info(
                req,
                ctx,
                NotifyCode::AuthResult,
                Some(&reply.line),
                reply.code,
            )?;
        } else {
            notify_error(
                req,
                ctx,
                NotifyCode::AuthResult,
                Some(&reply.line),
                reply.code,
            )?;
        }
    }
    Ok(reply.class() == Class::Completion)
}

/// The `ftps://` half of `php_ftp_fopen_connect`: `AUTH TLS`, or `AUTH SSL`
/// where the server refuses it, the control connection secured, `PBSZ 0`
/// and `PROT P`; whether the data connections are secured too, a `PROT P`
/// the server took or an `AUTH SSL` server's, and `None` where the server
/// takes neither `AUTH` or the handshake failed, after php's log.
fn secure_control(
    req: &mut Request,
    control: Stream,
    report: Report,
) -> Result<Option<bool>, Thrown> {
    command(req, control, b"AUTH TLS", None);
    // An old ftpd-ssl answers `AUTH SSL` and wants its session reused,
    // which php marks and never hands on.
    let mut reuse = false;
    if get_result(req, control)?.code != 234 {
        command(req, control, b"AUTH SSL", None);
        if get_result(req, control)?.code != 334 {
            log(req, report, b"Server doesn't support FTPS.");
            return Ok(None);
        }
        reuse = true;
    }
    if !secured(req, control) {
        log(req, report, b"Unable to activate SSL mode");
        return Ok(None);
    }
    command(req, control, b"PBSZ 0", None);
    get_result(req, control)?;
    command(req, control, b"PROT P", None);
    Ok(Some(
        get_result(req, control)?.class() == Class::Completion || reuse,
    ))
}

/// A data connection the server opened: `php_stream_context_set` of the
/// `context`, and, where `ssl_on_data`, its crypto set up and enabled; a
/// connection that could not be secured is `false` after php's log, for
/// the caller to close with the owner it holds.
fn data_secured(
    req: &mut Request,
    data: Stream,
    ctx: Option<StreamCtx>,
    ssl_on_data: bool,
) -> bool {
    data.set_ctx(req, ctx);
    if ssl_on_data && !secured(req, data) {
        req.wrapper_log_error("Unable to activate SSL mode");
        return false;
    }
    true
}

/// Where the server listens for the data connection:
/// `php_fopen_do_pasv`'s `port`, and the host a `PASV` reply named,
/// where it named one.
struct Passive {
    host: Option<Vec<u8>>,
    port: u16,
}

/// `php_fopen_do_pasv` (`Z/ext/standard/ftp_fopen_wrapper.c:321`):
/// `EPSV` first, its `229`'s `port` between the third and fourth `|`;
/// otherwise `PASV`, whose `227` names four numbers of the host and two
/// of the `port`. `None` where neither answered one to read.
fn passive(req: &mut Request, control: Stream) -> Result<Option<Passive>, Thrown> {
    command(req, control, b"EPSV", None);
    let reply = get_result(req, control)?;
    if reply.code == 229 {
        let line = c_str(&reply.line);
        let mut pipes = 0;
        let mut at = 4.min(line.len());
        while at < line.len() {
            if line[at] == b'|' {
                pipes += 1;
                if pipes == 3 {
                    break;
                }
            }
            at += 1;
        }
        if pipes < 3 {
            return Ok(None);
        }
        let port = strtoul_port(&line[at + 1..]).0;
        return Ok((port != 0).then_some(Passive { host: None, port }));
    }
    command(req, control, b"PASV", None);
    let reply = get_result(req, control)?;
    if reply.code != 227 {
        return Ok(None);
    }
    let line = c_str(&reply.line);
    let Some(start) = line
        .get(4..)
        .and_then(|rest| rest.iter().position(u8::is_ascii_digit))
        .map(|at| at + 4)
    else {
        return Ok(None);
    };
    let mut at = start;
    let mut host = Vec::new();
    for _ in 0..4 {
        let digits = line[at..].iter().take_while(|b| b.is_ascii_digit()).count();
        if line.get(at + digits) != Some(&b',') {
            return Ok(None);
        }
        host.extend_from_slice(&line[at..at + digits]);
        host.push(b'.');
        at += digits + 1;
    }
    host.pop();
    // `ip` is a `char[16]`, `sizeof("123.123.123.123")`.
    host.truncate(15);
    let (high, used) = strtoul_port(&line[at..]);
    at += used;
    if line.get(at) != Some(&b',') {
        return Ok(None);
    }
    let (low, _) = strtoul_port(&line[at + 1..]);
    let port = high.wrapping_mul(256).wrapping_add(low);
    Ok((port != 0).then_some(Passive {
        host: Some(host),
        port,
    }))
}

/// `(unsigned short) strtoul(s, &end, 10)`: the number and how many
/// bytes it took. Kept beside the face's `strtol`, which answers no end
/// and saturates a signed long; the white space it skips is the face's.
fn strtoul_port(s: &[u8]) -> (u16, usize) {
    let blanks = s.iter().take_while(|b| is_space(**b)).count();
    let digits = s[blanks..]
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    let n = s[blanks..blanks + digits].iter().fold(0u64, |n, d| {
        n.wrapping_mul(10).wrapping_add(u64::from(d - b'0'))
    });
    #[allow(clippy::cast_possible_truncation)] // C's `(unsigned short)` of the `unsigned long`
    let port = n as u16;
    (port, if digits == 0 { 0 } else { blanks + digits })
}

/// What the stream is opened for, `read_write` of
/// `php_stream_url_wrap_ftp`: reading, writing a new file, or appending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Read,
    Write,
    Append,
}

impl Transfer {
    /// The `mode` as php sorts it: an `r` or `+` reads, a `w`, `a` or
    /// `+` writes, an `a` appends; both is php's refusal, and neither.
    fn of(mode: &[u8]) -> Result<Transfer, &'static str> {
        let reads = mode.iter().any(|b| matches!(b, b'r' | b'+'));
        let writes = mode.iter().any(|b| matches!(b, b'w' | b'a' | b'+'));
        match (reads, writes) {
            (true, true) => Err("FTP does not support simultaneous read/write connections"),
            (true, false) => Ok(Transfer::Read),
            (false, true) if mode.contains(&b'a') => Ok(Transfer::Append),
            (false, true) => Ok(Transfer::Write),
            (false, false) => Err("Unknown file open mode"),
        }
    }

    const fn verb(self) -> &'static [u8] {
        match self {
            Transfer::Read => b"RETR",
            Transfer::Write => b"STOR",
            Transfer::Append => b"APPE",
        }
    }
}

/// The control connection a data stream keeps, `stream->wrapperthis`,
/// with what the transfer was: `php_stream_ftp_stream_close` reads the
/// reply that ends an upload, warns where it is no `226` or `250`, and
/// quits.
struct Control {
    stream: Owner<Stream>,
    transfer: Transfer,
}

impl Closer for Control {
    fn close(self: Box<Self>, req: &mut Request) -> Result<(), Thrown> {
        let Control { stream, transfer } = *self;
        closing(req, stream, |req, stream| {
            if transfer != Transfer::Read {
                let reply = get_result(req, stream)?;
                if reply.code != 226 && reply.code != 250 {
                    let text = format!(
                        "FTP server error {}:{}",
                        reply.code,
                        String::from_utf8_lossy(c_str(&reply.line))
                    );
                    docref(req, Level::Warning, &text);
                }
            }
            command(req, stream, b"QUIT", None);
            Ok(())
        })
    }
}

/// What the exchange before a transfer came to: the last reply and the
/// size `SIZE` gave a read, or the reply that refused it.
enum Prepared {
    Ready { last: Reply, file_size: usize },
    Refused(Reply),
}

/// The exchange `php_stream_url_wrap_ftp` makes before the passive
/// connection: the transfer set binary, then `SIZE`, which a read needs
/// answered and tells the notifier of, and which refuses a write over a
/// file that is there unless `overwrite` says to `DELE` it first.
fn prepare(
    req: &mut Request,
    control: Stream,
    ctx: Option<StreamCtx>,
    transfer: Transfer,
    file: &[u8],
) -> Result<Prepared, Thrown> {
    command(req, control, b"TYPE I", None);
    let last = get_result(req, control)?;
    if last.class() != Class::Completion {
        return Ok(Prepared::Refused(last));
    }
    command(req, control, b"SIZE", Some(file));
    let last = get_result(req, control)?;
    match transfer {
        Transfer::Read => {
            if last.class() != Class::Completion {
                return Ok(Prepared::Refused(last));
            }
            let mut file_size = 0;
            if let Some(space) = c_str(&last.line).iter().position(|b| *b == b' ') {
                // `size_t file_size = atoi(...)`: a negative `int` is the
                // size it wraps to.
                file_size =
                    usize::try_from(i64::from(atoi(&last.line[space + 1..])).cast_unsigned())
                        .unwrap_or(usize::MAX);
                notify(
                    req,
                    ctx,
                    Notification::FileSize {
                        size: file_size,
                        message: c_str(&last.line),
                        message_code: last.code,
                    },
                )?;
            }
            Ok(Prepared::Ready { last, file_size })
        }
        Transfer::Write => {
            let overwrite = match ctx {
                Some(ctx) => match ctx.option(req, b"ftp", b"overwrite") {
                    Some(value) => truthy(req, &value)?,
                    None => false,
                },
                None => false,
            };
            if last.class() != Class::Completion {
                return Ok(Prepared::Ready { last, file_size: 0 });
            }
            if !overwrite {
                req.wrapper_log_error(
                    "Remote file already exists and overwrite context option not specified",
                );
                return Ok(Prepared::Refused(last));
            }
            command(req, control, b"DELE", Some(file));
            let deleted = get_result(req, control)?;
            if deleted.class() != Class::Completion {
                return Ok(Prepared::Refused(deleted));
            }
            Ok(Prepared::Ready {
                last: deleted,
                file_size: 0,
            })
        }
        Transfer::Append => Ok(Prepared::Ready { last, file_size: 0 }),
    }
}

/// `php_stream_url_wrap_ftp` (`Z/ext/standard/ftp_fopen_wrapper.c:411`):
/// a read through the `context`'s `proxy` handed to the http wrapper;
/// otherwise the session made, the transfer set binary, the file's size
/// asked (a read needs it to exist, a write refuses one that does unless
/// `overwrite` says to delete it), the data connection made passively,
/// `REST` sent for a `resume_pos`, and the transfer started, the data
/// stream answered with the control connection kept for its close.
///
/// ```php
/// var_dump(in_array('ftp', stream_get_wrappers()));
/// var_dump(@fopen('ftp://127.0.0.1:1/x', 'r+'), error_get_last()['message']);
/// var_dump(@fopen('ftp://127.0.0.1:1/x', 'x'), error_get_last()['message']);
/// $ctx = stream_context_create(['ftp' => ['proxy' => 'tcp://127.0.0.1:1']]);
/// var_dump(@fopen('ftp://127.0.0.1:1/x', 'w', false, $ctx), error_get_last()['message']);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// string(%d) "fopen(ftp://127.0.0.1:1/x): Failed to open stream: FTP does not support simultaneous read/write connections"
/// bool(false)
/// string(%d) "fopen(ftp://127.0.0.1:1/x): Failed to open stream: Unknown file open mode"
/// bool(false)
/// string(%d) "fopen(ftp://127.0.0.1:1/x): Failed to open stream: FTP proxy may only be used in read mode"
/// ```
fn open(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let transfer = match Transfer::of(mode) {
        Ok(transfer) => transfer,
        Err(refusal) => {
            req.wrapper_log_error(refusal);
            return Ok(None);
        }
    };
    let ctx = req.stream_ctx();
    if let Some(ctx) = ctx
        && ctx.option(req, b"ftp", b"proxy").is_some()
    {
        if transfer == Transfer::Read {
            return url_wrap_http(req, "ftp", path, mode, opening);
        }
        req.wrapper_log_error("FTP proxy may only be used in read mode");
        return Ok(None);
    }
    let Some(Session {
        control,
        resource,
        ssl_on_data,
    }) = connect(req, path, ctx, Report::Log)?
    else {
        return Ok(None);
    };
    let started = start(req, *control, ctx, transfer, &resource, ssl_on_data);
    let data = match started {
        Ok(Started::Data(data)) => data,
        Ok(Started::Refused(reply, data_error)) => {
            refused(req, ctx, control, &reply, data_error.as_deref())?;
            return Ok(None);
        }
        Err(thrown) => {
            control.free(req);
            return Err(thrown);
        }
    };
    let closer = Box::new(Control {
        stream: control,
        transfer,
    });
    if let Err(thrown) = data.set_closer(req, closer) {
        data.free(req);
        return Err(thrown);
    }
    Ok(Some(data))
}

/// What the transfer's start came to: the data connection, or the reply
/// that refused it, with the reason a data connection was not made.
enum Started {
    Data(Owner<Stream>),
    Refused(Reply, Option<String>),
}

/// `errexit` of `php_stream_url_wrap_ftp` and `opendir_errexit`: the
/// notifier told of the reply, the control connection closed, and the
/// server's line and the data connection's failure logged.
fn refused(
    req: &mut Request,
    ctx: Option<StreamCtx>,
    control: Owner<Stream>,
    reply: &Reply,
    data_error: Option<&str>,
) -> Result<(), Thrown> {
    let notified = notify_error(req, ctx, NotifyCode::Failure, Some(&reply.line), reply.code);
    control.free(req);
    notified?;
    if !c_str(&reply.line).is_empty() {
        let mut message = b"FTP server reports ".to_vec();
        message.extend_from_slice(c_str(&reply.line));
        req.wrapper_log_error(&String::from_utf8_lossy(&message));
    }
    if let Some(text) = data_error {
        req.wrapper_log_error(&format!("Failed to set up data channel: {text}"));
    }
    Ok(())
}

/// The exchange of `php_stream_url_wrap_ftp` over the logged-in
/// `control`: the transfer prepared, the passive data connection made,
/// `REST` sent for a `resume_pos`, and the transfer started.
fn start(
    req: &mut Request,
    control: Stream,
    ctx: Option<StreamCtx>,
    transfer: Transfer,
    resource: &UriStruct,
    ssl_on_data: bool,
) -> Result<Started, Thrown> {
    let file = resource.path.clone().unwrap_or_default();
    let (mut last, file_size) = match prepare(req, control, ctx, transfer, &file)? {
        Prepared::Ready { last, file_size } => (last, file_size),
        Prepared::Refused(reply) => return Ok(Started::Refused(reply, None)),
    };
    let Some(passive) = passive(req, control)? else {
        return Ok(Started::Refused(last, None));
    };
    if transfer == Transfer::Read
        && let Some(ctx) = ctx
        && let Some(value) = ctx.option(req, b"ftp", b"resume_pos")
        && let ValueRef::Int(at) = value.as_ref()
        && at > 0
    {
        command(req, control, b"REST", Some(Decimal::of(at).as_bytes()));
        last = get_result(req, control)?;
        if last.class() != Class::Intermediate {
            req.wrapper_log_error(&format!("Unable to resume from offset {at}"));
            return Ok(Started::Refused(last, None));
        }
    }
    command(req, control, transfer.verb(), Some(&file));
    let host = passive
        .host
        .unwrap_or_else(|| c_str(resource.host.as_deref().unwrap_or_default()).to_vec());
    let mut transport = b"tcp://".to_vec();
    transport.extend_from_slice(&host);
    transport.extend_from_slice(format!(":{}", passive.port).as_bytes());
    let timeout = timeval(default_timeout(req).trunc());
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent: None,
        socket: SocketOptions::default(),
        context: ctx,
    };
    let data = match req.xport_create(&transport, xport)? {
        Ok(data) => data,
        Err(XportFailure { text, .. }) => {
            return Ok(Started::Refused(Reply::default(), text));
        }
    };
    let reply = match get_result(req, control) {
        Ok(reply) => reply,
        Err(thrown) => {
            data.free(req);
            return Err(thrown);
        }
    };
    if reply.code != 150 && reply.code != 125 {
        data.free(req);
        return Ok(Started::Refused(reply, None));
    }
    let init = Notification::ProgressInit {
        sofar: 0,
        max: file_size,
    };
    if let Err(thrown) = notify(req, ctx, init) {
        data.free(req);
        return Err(thrown);
    }
    if !data_secured(req, *data, ctx, ssl_on_data) {
        data.free(req);
        return Ok(Started::Refused(Reply::default(), None));
    }
    Ok(Started::Data(data))
}

/// `php_stream_ftp_opendir` (`Z/ext/standard/ftp_fopen_wrapper.c:689`):
/// the session made, the transfer set to ASCII, the data connection made
/// passively, and `NLST` of the path read off it whole, each line's base
/// name cut as `php_ftp_dirstream_read` cuts it, both connections closed
/// with no `QUIT`.
///
/// ```php
/// var_dump(@opendir('ftp://127.0.0.1:1/'), error_get_last()['message']);
/// ```
/// ```expect
/// bool(false)
/// string(71) "opendir(ftp://127.0.0.1:1/): Failed to open directory: operation failed"
/// ```
fn open_dir(req: &mut Request, path: &[u8]) -> Result<Option<DirListing>, Thrown> {
    let ctx = req.stream_ctx();
    let Some(Session {
        control,
        resource,
        ssl_on_data,
    }) = connect(req, path, ctx, Report::Log)?
    else {
        return Ok(None);
    };
    match listing(req, *control, ctx, &resource, ssl_on_data) {
        Ok(Listed::Names(names, data)) => {
            control.free(req);
            data.free(req);
            Ok(Some(DirListing::Names(names)))
        }
        Ok(Listed::Refused(reply)) => {
            refused(req, ctx, control, &reply, None)?;
            Ok(None)
        }
        Err(thrown) => {
            control.free(req);
            Err(thrown)
        }
    }
}

/// What the listing came to: the names `NLST` listed and the data
/// connection they were read off, or the reply that refused it.
enum Listed {
    Names(Vec<Vec<u8>>, Owner<Stream>),
    Refused(Reply),
}

/// The exchange of `php_stream_ftp_opendir` over the logged-in
/// `control`: the names `NLST` listed and the data connection they were
/// read off, or the reply that refused the listing.
fn listing(
    req: &mut Request,
    control: Stream,
    ctx: Option<StreamCtx>,
    resource: &UriStruct,
    ssl_on_data: bool,
) -> Result<Listed, Thrown> {
    command(req, control, b"TYPE A", None);
    let reply = get_result(req, control)?;
    if reply.class() != Class::Completion {
        return Ok(Listed::Refused(reply));
    }
    let Some(passive) = passive(req, control)? else {
        return Ok(Listed::Refused(Reply::default()));
    };
    let host = passive
        .host
        .unwrap_or_else(|| c_str(resource.host.as_deref().unwrap_or_default()).to_vec());
    let mut transport = host;
    transport.extend_from_slice(format!(":{}", passive.port).as_bytes());
    let timeout = timeval(default_timeout(req).trunc());
    let socket = SocketOptions::default();
    let xport = Xport::Client {
        timeout,
        connect: true,
        persistent: None,
        socket,
        context: None,
    };
    let data = match req.xport_create(&transport, xport)? {
        Ok(data) => data,
        Err(failed) => {
            docref(req, Level::Warning, &failed.warning());
            return Ok(Listed::Refused(Reply::default()));
        }
    };
    let listed = resource.path.clone().unwrap_or_else(|| b"/".to_vec());
    command(req, control, b"NLST", Some(&listed));
    let reply = match get_result(req, control) {
        Ok(reply) => reply,
        Err(thrown) => {
            data.free(req);
            return Err(thrown);
        }
    };
    if reply.code != 150 && reply.code != 125 {
        data.free(req);
        return Ok(Listed::Refused(reply));
    }
    if !data_secured(req, *data, ctx, ssl_on_data) {
        data.free(req);
        return Ok(Listed::Refused(Reply::default()));
    }
    let mut names = Vec::new();
    while !data.eof(req) {
        let line = match data.get_line(req, Some(MAXPATHLEN)) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(thrown) => {
                data.free(req);
                return Err(thrown);
            }
        };
        names.push(entry_name(&line));
    }
    Ok(Listed::Names(names, data))
}

/// `php_ftp_dirstream_read`'s cut of one listed line: its base name,
/// `php_basename`, less the last two bytes, a line's `\r\n`, as php's
/// `tmp_len - 1` terminator lands; a lone `\n` so costs the name's last
/// byte, as php's does.
fn entry_name(line: &[u8]) -> Vec<u8> {
    let base = crate::string::path::php_basename(line, b"");
    let kept = base.len().min(MAXPATHLEN).saturating_sub(2);
    c_str(&base[..kept]).to_vec()
}

/// The thirteen fields of a stat as the array the engine reads an
/// extension's `url_stat` from.
const STAT_NAMES: [&[u8]; 13] = [
    b"dev", b"ino", b"mode", b"nlink", b"uid", b"gid", b"rdev", b"size", b"atime", b"mtime",
    b"ctime", b"blksize", b"blocks",
];

/// `S_IFREG`, `S_IFDIR` and `S_IXUSR | S_IXGRP | S_IXOTH`, the bits a directory is given.
const S_IFREG: i64 = 0o100_000;
const S_IFDIR: i64 = 0o040_000;
const S_IXALL: i64 = 0o111;

/// `php_stream_ftp_url_stat` (`Z/ext/standard/ftp_fopen_wrapper.c:781`):
/// a directory where `CWD` to the path is taken, a file otherwise, both
/// `0644`; the size `SIZE` answers, none of a directory's, and a file
/// with none is no file; the modification time `MDTM` answers, taken as
/// UTC; and the rest php guesses. `None` where the session or the size
/// failed.
///
/// ```php
/// var_dump(@stat('ftp://127.0.0.1:1/x'));
/// ```
/// ```expect
/// bool(false)
/// ```
fn url_stat(req: &mut Request, url: &[u8]) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let ctx = req.stream_ctx();
    let Some(Session {
        control, resource, ..
    }) = connect(req, url, ctx, Report::Quiet)?
    else {
        return Ok(None);
    };
    let file = resource.path.clone().unwrap_or_else(|| b"/".to_vec());
    let stated = closing(req, control, |req, control| {
        let mut mode = 0o644;
        command(req, control, b"CWD", Some(&file));
        let reply = get_result(req, control)?;
        let dir = reply.class() == Class::Completion;
        mode |= if dir { S_IFDIR | S_IXALL } else { S_IFREG };
        command(req, control, b"TYPE I", None);
        if get_result(req, control)?.class() != Class::Completion {
            return Ok(None);
        }
        command(req, control, b"SIZE", Some(&file));
        let reply = get_result(req, control)?;
        let size = if reply.class() == Class::Completion {
            i64::from(atoi(reply.line.get(4..).unwrap_or_default()))
        } else if dir {
            0
        } else {
            return Ok(None);
        };
        command(req, control, b"MDTM", Some(&file));
        let reply = get_result(req, control)?;
        let mtime = if reply.code == 213 {
            mdtm(&reply.line)
        } else {
            -1
        };
        Ok(Some((mode, size, mtime)))
    })?;
    let Some((mode, size, mtime)) = stated else {
        return Ok(None);
    };
    let blocks = (4095 + size) / 4096;
    let sb = [0, 0, mode, 1, 0, 0, -1, size, -1, mtime, -1, 4096, blocks];
    let mut out = PhpArray::new(req)?;
    for (name, field) in STAT_NAMES.iter().zip(sb) {
        out.set_str(req, name, Owner::int(field))?;
    }
    Ok(Some(out))
}

/// The time `MDTM` answers, `YYYYMMDDhhmmss` after the code, as a Unix
/// time: php reads the fields with `sscanf("%4d%2d%2d%2d%2d%2d")` and
/// takes them for UTC through `mktime` and the host's offset, which is
/// the Unix time of that UTC date but across a change of the host's
/// daylight saving; `-1` where six fields are not there.
fn mdtm(line: &[u8]) -> i64 {
    let rest = line.get(4..).unwrap_or_default();
    let Some(start) = rest.iter().position(u8::is_ascii_digit) else {
        return -1;
    };
    let mut at = start;
    let mut fields = [0i64; 6];
    for (field, width) in fields.iter_mut().zip([4, 2, 2, 2, 2, 2]) {
        let digits = rest[at..]
            .iter()
            .take(width)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if digits == 0 {
            return -1;
        }
        *field = strtol(&rest[at..at + digits]);
        at += digits;
    }
    let [year, month, day, hour, minute, second] = fields;
    days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar,
/// `mktime`'s normalisation of a month past twelve or a day past the
/// month's end included.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let (year, month) = (
        year + (month - 1).div_euclid(12),
        (month - 1).rem_euclid(12) + 1,
    );
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `php_stream_ftp_unlink` (`Z/ext/standard/ftp_fopen_wrapper.c:903`):
/// `DELE`, php's warnings under `REPORT_ERRORS` where the session, the
/// path or the deletion fails.
///
/// ```php
/// var_dump(unlink('ftp://127.0.0.1:1/x'));
/// ```
/// ```expect
/// Warning: unlink(): connect() failed: Connection refused in %s on line 2
///
/// Warning: unlink(): Unable to connect to ftp://127.0.0.1:1/x in %s on line 2
/// bool(false)
/// ```
fn unlink(req: &mut Request, url: &[u8]) -> Result<bool, Thrown> {
    let ctx = req.stream_ctx();
    let Some(Session {
        control, resource, ..
    }) = connect(req, url, ctx, Report::Quiet)?
    else {
        let text = format!(
            "Unable to connect to {}",
            String::from_utf8_lossy(c_str(url))
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    let file = resource.path.clone().unwrap_or_default();
    let reply = closing(req, control, |req, control| {
        command(req, control, b"DELE", Some(&file));
        get_result(req, control)
    })?;
    if reply.class() != Class::Completion {
        let text = format!(
            "Error Deleting file: {}",
            String::from_utf8_lossy(c_str(&reply.line))
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    Ok(true)
}

/// `php_stream_ftp_rename` (`Z/ext/standard/ftp_fopen_wrapper.c:952`):
/// both URLs of one scheme, host and `port` (21 and none are one), each
/// with a path; then `RNFR` and `RNTO`, php's warnings under
/// `REPORT_ERRORS` where the session or either fails.
///
/// ```php
/// var_dump(rename('ftp://127.0.0.1:1/x', 'ftp://127.0.0.2:1/y'));
/// ```
/// ```expect
/// bool(false)
/// ```
fn rename(req: &mut Request, from: &[u8], to: &[u8]) -> Result<bool, Thrown> {
    let ctx = req.stream_ctx();
    let Some(parser) = UriParser::of_ctx(req, ctx, b"ftp")? else {
        let text = format!(
            "{}(): Provided stream context has invalid value for the \"uri_parser_class\" option",
            req.frames().active_function_name()
        );
        return Err(throw_error(req, "ValueError", &text));
    };
    let Some(source) = parser.parse(req, c_str(from))? else {
        return Ok(false);
    };
    let Some(target) = parser.parse(req, c_str(to))? else {
        return Ok(false);
    };
    let (from_port, to_port) = (source.port.unwrap_or(0), target.port.unwrap_or(0));
    let same = source.scheme.is_some()
        && source.scheme == target.scheme
        && source.host.is_some()
        && source.host == target.host
        && (from_port == to_port || from_port * to_port == 0 || from_port + to_port == 21)
        && source.path.is_some()
        && target.path.is_some();
    if !same {
        return Ok(false);
    }
    let Some(Session { control, .. }) = connect(req, from, ctx, Report::Quiet)? else {
        let host = source.host.as_deref().map(c_str).unwrap_or_default();
        let text = format!("Unable to connect to {}", String::from_utf8_lossy(host));
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    let reply = closing(req, control, |req, control| {
        command(req, control, b"RNFR", source.path.as_deref());
        let reply = get_result(req, control)?;
        if reply.class() != Class::Intermediate {
            return Ok(Err(reply));
        }
        command(req, control, b"RNTO", target.path.as_deref());
        get_result(req, control).map(Ok)
    })?;
    let reply = match reply {
        Ok(reply) => reply,
        Err(reply) => {
            let text = format!(
                "Error Renaming file: {}",
                String::from_utf8_lossy(c_str(&reply.line))
            );
            docref(req, Level::Warning, &text);
            return Ok(false);
        }
    };
    if reply.class() != Class::Completion {
        let text = format!(
            "Error Renaming file: {}",
            String::from_utf8_lossy(c_str(&reply.line))
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    Ok(true)
}

/// `php_stream_ftp_mkdir` (`Z/ext/standard/ftp_fopen_wrapper.c:1040`):
/// `MKD` of the path; recursively, `CWD` back up the path from its end
/// to the first directory that is there, then `MKD` of each below it,
/// php's warning where one is refused. The walk is php's over the path
/// as a C string it cuts at each `/` it tries, so a trailing `/` is
/// tried first and made last.
///
/// ```php
/// var_dump(mkdir('ftp://127.0.0.1:1/a/b', 0755, true));
/// ```
/// ```expect
/// Warning: mkdir(): connect() failed: Connection refused in %s on line 2
///
/// Warning: mkdir(): Unable to connect to ftp://127.0.0.1:1/a/b in %s on line 2
/// bool(false)
/// ```
fn mkdir(req: &mut Request, url: &[u8], _mode: i64, recursive: bool) -> Result<bool, Thrown> {
    let ctx = req.stream_ctx();
    let Some(Session {
        control, resource, ..
    }) = connect(req, url, ctx, Report::Quiet)?
    else {
        let text = format!(
            "Unable to connect to {}",
            String::from_utf8_lossy(c_str(url))
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    let path = c_str(resource.path.as_deref().unwrap_or_default()).to_vec();
    let reply = closing(req, control, |req, control| {
        if recursive {
            make_parents(req, control, &path)
        } else {
            command(req, control, b"MKD", Some(&path));
            get_result(req, control)
        }
    })?;
    Ok(reply.class() == Class::Completion)
}

/// The recursive arm of `php_stream_ftp_mkdir`, over `buf`, the path with
/// its NUL: each `/` from the end cut to a NUL and `CWD` asked of what is
/// left, `/` for nothing, until one is taken; `MKD` of that; then every
/// NUL the walk left, but the last, turned back into a `/` and `MKD` of
/// the path up to the next NUL. The last reply answered.
fn make_parents(req: &mut Request, control: Stream, path: &[u8]) -> Result<Reply, Thrown> {
    let mut buf = path.to_vec();
    let e = buf.len();
    buf.push(0);
    let shown = |buf: &[u8]| {
        let upto = c_str(buf);
        if upto.is_empty() {
            b"/".to_vec()
        } else {
            upto.to_vec()
        }
    };
    let mut found = None;
    while let Some(p) = c_str(&buf).iter().rposition(|b| *b == b'/') {
        buf[p] = 0;
        command(req, control, b"CWD", Some(&shown(&buf)));
        if get_result(req, control)?.class() == Class::Completion {
            buf[p] = b'/';
            found = Some(p);
            break;
        }
    }
    command(req, control, b"MKD", Some(&shown(&buf)));
    let mut reply = get_result(req, control)?;
    if reply.class() == Class::Completion {
        let mut p = found.unwrap_or(0);
        while p != e {
            if buf[p] == 0 && buf[p + 1] != 0 {
                buf[p] = b'/';
                command(req, control, b"MKD", Some(c_str(&buf)));
                reply = get_result(req, control)?;
                if reply.class() != Class::Completion {
                    let text = String::from_utf8_lossy(c_str(&reply.line)).into_owned();
                    docref(req, Level::Warning, &text);
                    break;
                }
            }
            p += 1;
        }
    }
    Ok(reply)
}

/// `php_stream_ftp_rmdir` (`Z/ext/standard/ftp_fopen_wrapper.c:1132`):
/// `RMD` of the path, the refusal's line php's warning.
///
/// ```php
/// var_dump(rmdir('ftp://127.0.0.1:1/a'));
/// ```
/// ```expect
/// Warning: rmdir(): connect() failed: Connection refused in %s on line 2
///
/// Warning: rmdir(): Unable to connect to ftp://127.0.0.1:1/a in %s on line 2
/// bool(false)
/// ```
fn rmdir(req: &mut Request, url: &[u8]) -> Result<bool, Thrown> {
    let ctx = req.stream_ctx();
    let Some(Session {
        control, resource, ..
    }) = connect(req, url, ctx, Report::Quiet)?
    else {
        let text = format!(
            "Unable to connect to {}",
            String::from_utf8_lossy(c_str(url))
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    };
    let path = resource.path.clone().unwrap_or_default();
    let reply = closing(req, control, |req, control| {
        command(req, control, b"RMD", Some(&path));
        get_result(req, control)
    })?;
    if reply.class() != Class::Completion {
        let text = String::from_utf8_lossy(c_str(&reply.line)).into_owned();
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    Ok(true)
}

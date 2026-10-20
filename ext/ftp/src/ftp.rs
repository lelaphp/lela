//! `ftp.c`: one FTP session over the engine's sockets. The control
//! connection is a socket stream `xport_create` connects, a data
//! connection one it connects (passive) or accepts (active), and FTPS is
//! the crypto layer the bundle's `openssl` registers for `tcp`, set up on
//! the control connection and on each data connection with the control
//! connection's session. What php reads and writes with `recv(2)`,
//! `send(2)` and `poll(2)` under `timeout_sec` is a stream read under
//! that read timeout and a stream write.

use std::time::Duration;

use php_ext::coerce::{c_str, strtol};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    Handshake, OptionAnswer, OwnedStream as _, Request, SocketOptions, Stream, StreamCtx,
    StreamOption, Streams as _, Xport, XportFailure,
};
use php_ext::values::{Owner, PhpArray, PhpString, Value};

/// `FTP_BUFSIZE`: the most one read asks for and the longest line.
pub(crate) const BUFSIZE: usize = 4096;

/// `STREAM_CRYPTO_METHOD_SSLv23_CLIENT`.
const SSLV23_CLIENT: i64 = (1 << 3) | (1 << 4) | (1 << 5) | 1;

/// A reply code, decoded once from the three digits `ftp_getresp`
/// reads: the ones `ftp.c` tells apart by name, any other by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Code {
    /// 125: the data connection is already open, the transfer starts.
    AlreadyOpen,
    /// 150: the data connection is about to open.
    Opening,
    /// 200: the command was taken.
    Okay,
    /// 213: a file's status, `SIZE` and `MDTM`.
    FileStatus,
    /// 215: the system type.
    System,
    /// 220: ready for a new user.
    Ready,
    /// 221: the server closes the control connection.
    Closing,
    /// 226: the transfer is done and the data connection closed.
    Transferred,
    /// 227: passive, with the address to connect to.
    Passive,
    /// 229: extended passive, with the number to connect to.
    ExtendedPassive,
    /// 230: the user is logged in.
    LoggedIn,
    /// 234: `AUTH TLS` taken.
    AuthTls,
    /// 250: the file action is done.
    FileDone,
    /// 257: the path, `PWD` and `MKD`.
    Path,
    /// 331: the user name is taken, the password is wanted.
    NeedPassword,
    /// 334: `AUTH SSL` taken.
    AuthSsl,
    /// 350: the action waits for what follows, `REST` and `RNFR`.
    Pending,
    /// Any other.
    Other(u16),
}

impl Code {
    /// The code of three digits.
    const fn of(n: u16) -> Code {
        match n {
            125 => Code::AlreadyOpen,
            150 => Code::Opening,
            200 => Code::Okay,
            213 => Code::FileStatus,
            215 => Code::System,
            220 => Code::Ready,
            221 => Code::Closing,
            226 => Code::Transferred,
            227 => Code::Passive,
            229 => Code::ExtendedPassive,
            230 => Code::LoggedIn,
            234 => Code::AuthTls,
            250 => Code::FileDone,
            257 => Code::Path,
            331 => Code::NeedPassword,
            334 => Code::AuthSsl,
            350 => Code::Pending,
            n => Code::Other(n),
        }
    }

    /// `resp >= 200 && resp < 300`, a positive completion.
    const fn completed(self) -> bool {
        match self {
            Code::Other(n) => n >= 200 && n < 300,
            Code::Okay
            | Code::FileStatus
            | Code::System
            | Code::Ready
            | Code::Closing
            | Code::Transferred
            | Code::Passive
            | Code::ExtendedPassive
            | Code::LoggedIn
            | Code::AuthTls
            | Code::FileDone
            | Code::Path => true,
            _ => false,
        }
    }
}

/// `ftptype_t`: how a transfer treats line ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `FTPTYPE_ASCII`, `TYPE A`.
    Ascii,
    /// `FTPTYPE_IMAGE`, `TYPE I`.
    Image,
}

/// `pasv`: off, asked for, or asked for and answered with the address the
/// next data connection goes to (`pasv == 2`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pasv {
    Off,
    On,
    Ready(String),
}

/// FTPS on the session: none, asked for by `ftp_ssl_connect` and set up at
/// login, or up, with whether the data connections are secured too, and
/// the `context` whose `ssl` options every connection is made under.
#[derive(Debug)]
enum Tls {
    Off,
    Wanted,
    Up { data: bool },
}

/// A data connection, `databuf_t`: a socket listening for the server to
/// connect (active), or one connected (passive, or accepted).
pub(crate) enum Data {
    Listening(Owner<Stream>),
    Connected(Owner<Stream>),
}

/// Which way a non-blocking transfer runs, `direction`, with what a
/// receive in ASCII carries across reads, `lastch`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Way {
    Get { lastch: u8 },
    Put,
}

/// A non-blocking transfer in flight between `ftp_nb_*` and
/// `ftp_nb_continue`: `ftp->data`, `ftp->stream`, `direction` and
/// `closestream`, which php keeps as four fields beside `nb`; the
/// transfer holds an owner of each stream.
pub(crate) struct Nb {
    pub(crate) data: Owner<Stream>,
    pub(crate) stream: Owner<Stream>,
    pub(crate) way: Way,
    pub(crate) close: bool,
}

/// What `ftp_nb_*` and `ftp_nb_continue` answer: `PHP_FTP_FAILED`,
/// `PHP_FTP_FINISHED` and `PHP_FTP_MOREDATA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Progress {
    Failed = 0,
    Finished = 1,
    MoreData = 2,
}

/// One read of a socket under a timeout.
enum Recv {
    Data(Vec<u8>),
    Eof,
    Failed,
}

/// `ftpbuf_t`: one session.
pub(crate) struct Ftp {
    control: Owner<Stream>,
    /// The control connection's local address, `localaddr`.
    local: String,
    /// `resp`, `None` for php's `0`.
    resp: Option<Code>,
    /// `inbuf`: the last reply's text after its code, or what the read
    /// that failed had.
    pub(crate) inbuf: Vec<u8>,
    /// `extra`: bytes read past the last line.
    extra: Vec<u8>,
    pwd: Option<Vec<u8>>,
    syst: Option<Vec<u8>>,
    /// `type`, `None` before a `TYPE` was taken.
    kind: Option<Kind>,
    pasv: Pasv,
    pub(crate) timeout: i64,
    pub(crate) autoseek: bool,
    pub(crate) usepasvaddress: bool,
    tls: Tls,
    /// The `context` holding `verify_peer` off, as php's `SSL_CTX` checks
    /// no peer ([`unverified`]); an owner the session keeps.
    context: Option<Owner<StreamCtx>>,
    /// The transfer `ftp_nb_*` left in flight.
    pub(crate) nb: Option<Nb>,
}

/// `(unsigned short) port`, the low 16 bits.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn port16(n: i64) -> u16 {
    n as u16
}

/// The host of `ip:port` or `[ip]:port`, as `php_network_populate_name_from_sockaddr` writes it.
fn host_of(name: &[u8]) -> String {
    let text = String::from_utf8_lossy(name);
    let host = text.rsplit_once(':').map_or(&*text, |(host, _)| host);
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned()
}

/// `tcp://host:port`, the host bracketed where it is an IPv6 address.
fn transport(host: &str, port: u16) -> Vec<u8> {
    if host.contains(':') {
        format!("tcp://[{host}]:{port}").into_bytes()
    } else {
        format!("tcp://{host}:{port}").into_bytes()
    }
}

/// `timeout_sec` as a duration.
fn seconds(n: i64) -> Duration {
    Duration::from_secs(u64::try_from(n).unwrap_or(0))
}

/// `strerror(ETIMEDOUT)`, the warning a read that ran out of time
/// raises.
fn timed_out(req: &mut Request) {
    let text = req.environment().os_error_text(libc::ETIMEDOUT);
    docref(req, Level::Warning, &text);
}

/// Whether the stream's last read ran out of time, its `timed_out`.
fn ran_out(req: &mut Request, stream: Stream) -> Result<bool, Thrown> {
    let meta = stream.meta_data(req)?;
    let answer = meta
        .find(b"timed_out")
        .is_some_and(|v| v.as_bool() == Some(true));
    meta.give_up(req);
    Ok(answer)
}

/// `my_recv`: one read of at most `len` bytes within `timeout`, the
/// warning where it ran out of time.
fn recv(req: &mut Request, stream: Stream, len: usize, timeout: Duration) -> Result<Recv, Thrown> {
    if stream.set_option(req, StreamOption::ReadTimeout(timeout)) == OptionAnswer::Err {
        return Ok(Recv::Failed);
    }
    let read = stream.read(req, len)?;
    let bytes = read.map(|s| {
        let bytes = s.as_bytes().to_vec();
        s.give_up(req);
        bytes
    });
    match bytes {
        Some(bytes) if !bytes.is_empty() => Ok(Recv::Data(bytes)),
        _ if ran_out(req, stream)? => {
            timed_out(req);
            Ok(Recv::Failed)
        }
        Some(_) => Ok(Recv::Eof),
        None => Ok(Recv::Failed),
    }
}

/// `my_send`: all of `bytes`, or `false`. A peer that has closed takes
/// nothing: php's `send(2)` fails there without a word, where the
/// stream's write would raise its notice, so liveness is asked first.
fn send(req: &mut Request, stream: Stream, bytes: &[u8]) -> bool {
    if stream.set_option(req, StreamOption::Liveness) == OptionAnswer::Err {
        return false;
    }
    stream.write(req, bytes) == Some(bytes.len())
}

/// `php_stream_context_alloc` with `ssl`'s `verify_peer` and
/// `verify_peer_name` off and `allow_self_signed` on: the options php's
/// `SSL_CTX_new` has, which checks no peer; the one owner the caller's.
fn unverified(req: &mut Request) -> Result<Owner<StreamCtx>, Thrown> {
    let context = StreamCtx::alloc(req)?;
    for (option, value) in [
        (&b"verify_peer"[..], false),
        (b"verify_peer_name", false),
        (b"allow_self_signed", true),
    ] {
        if let Err(thrown) = context.set_option(req, b"ssl", option, &Value::bool(value)) {
            context.give_up(req);
            return Err(thrown);
        }
    }
    Ok(context)
}

impl Ftp {
    /// `ftp_open`: the control connection made within `timeout`
    /// seconds and the greeting read, `220` or nothing.
    pub(crate) fn open(
        req: &mut Request,
        host: &[u8],
        port: i64,
        timeout: i64,
        tls: bool,
    ) -> Result<Option<Ftp>, Thrown> {
        let port = match port16(port) {
            0 => 21,
            port => port,
        };
        let context = if tls { Some(unverified(req)?) } else { None };
        let mut name = b"tcp://".to_vec();
        name.extend_from_slice(c_str(host));
        let target = String::from_utf8_lossy(&name[6..]).into_owned();
        let mut name = if target.contains(':') {
            transport(&target, port)
        } else {
            name
        };
        if !target.contains(':') {
            name.extend_from_slice(format!(":{port}").as_bytes());
        }
        let xport = Xport::Client {
            timeout: Some(seconds(timeout)),
            connect: true,
            persistent: None,
            socket: SocketOptions::default(),
            context: context.as_ref().map(|context| **context),
        };
        let control = match req.xport_create(&name, xport) {
            Ok(Ok(control)) => control,
            made => {
                if let Some(context) = context {
                    context.give_up(req);
                }
                return made.map(|_| None);
            }
        };
        let name = match control.socket_name(req, false) {
            Ok(name) => name,
            Err(thrown) => {
                control.free(req);
                if let Some(context) = context {
                    context.give_up(req);
                }
                return Err(thrown);
            }
        };
        let local = name
            .map(|s| {
                let host = host_of(s.as_bytes());
                s.give_up(req);
                host
            })
            .unwrap_or_default();
        let mut ftp = Ftp {
            control,
            local,
            resp: None,
            inbuf: Vec::new(),
            extra: Vec::new(),
            pwd: None,
            syst: None,
            kind: None,
            pasv: Pasv::Off,
            timeout,
            autoseek: true,
            usepasvaddress: true,
            tls: if tls { Tls::Wanted } else { Tls::Off },
            context,
            nb: None,
        };
        match ftp.getresp(req) {
            Ok(true) if ftp.resp == Some(Code::Ready) => Ok(Some(ftp)),
            answer => {
                ftp.close(req);
                answer.map(|_| None)
            }
        }
    }

    /// `ftp_close`: the transfer in flight let go, the stream it was to
    /// close closed, and the control connection closed.
    pub(crate) fn close(mut self, req: &mut Request) {
        self.abort(req);
        self.control.free(req);
        if let Some(context) = self.context {
            context.give_up(req);
        }
    }

    /// The non-blocking transfer let go: its data connection closed and,
    /// where `ftp_nb_get` or `ftp_nb_put` opened it, its stream.
    pub(crate) fn abort(&mut self, req: &mut Request) {
        if let Some(nb) = self.nb.take() {
            nb.data.free(req);
            let_go(req, nb.stream, nb.close);
        }
    }

    /// The streams the session holds, for the collector.
    pub(crate) fn streams(&self) -> Vec<Value> {
        let mut held = vec![self.control.as_value()];
        if let Some(nb) = &self.nb {
            held.push(nb.data.as_value());
            held.push(nb.stream.as_value());
        }
        held
    }

    /// `ftp_quit`: `QUIT` and its `221`.
    pub(crate) fn quit(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        if !self.putcmd(req, b"QUIT", None) {
            return Ok(false);
        }
        if !self.getresp(req)? || self.resp != Some(Code::Closing) {
            return Ok(false);
        }
        self.pwd = None;
        Ok(true)
    }

    /// One command and its reply, the reply held to `ok`.
    fn command(
        &mut self,
        req: &mut Request,
        cmd: &[u8],
        args: Option<&[u8]>,
        ok: impl Fn(Code) -> bool,
    ) -> Result<bool, Thrown> {
        if !self.putcmd(req, cmd, args) {
            return Ok(false);
        }
        Ok(self.getresp(req)? && self.resp.is_some_and(ok))
    }

    /// `ftp_login`: for FTPS, `AUTH TLS` (or `AUTH SSL`), the handshake,
    /// `PBSZ 0` and `PROT P`; then `USER` and, where the server asks,
    /// `PASS`.
    pub(crate) fn login(
        &mut self,
        req: &mut Request,
        user: &[u8],
        pass: &[u8],
    ) -> Result<bool, Thrown> {
        if matches!(self.tls, Tls::Wanted) {
            let mut old = false;
            if !self.putcmd(req, b"AUTH", Some(b"TLS")) || !self.getresp(req)? {
                return Ok(false);
            }
            if self.resp != Some(Code::AuthTls) {
                if !self.command(req, b"AUTH", Some(b"SSL"), |c| c == Code::AuthSsl)? {
                    return Ok(false);
                }
                old = true;
            }
            if !secure(req, *self.control, None) {
                docref(req, Level::Warning, "SSL/TLS handshake failed");
                return Ok(false);
            }
            let mut data = old;
            if !old {
                if !self.putcmd(req, b"PBSZ", Some(b"0")) || !self.getresp(req)? {
                    return Ok(false);
                }
                if !self.putcmd(req, b"PROT", Some(b"P")) || !self.getresp(req)? {
                    return Ok(false);
                }
                data = self.resp.is_some_and(Code::completed);
            }
            self.tls = Tls::Up { data };
        }
        if !self.putcmd(req, b"USER", Some(user)) || !self.getresp(req)? {
            return Ok(false);
        }
        if self.resp == Some(Code::LoggedIn) {
            return Ok(true);
        }
        if self.resp != Some(Code::NeedPassword) {
            return Ok(false);
        }
        self.command(req, b"PASS", Some(pass), |c| c == Code::LoggedIn)
    }

    /// `ftp_syst`: the first word of the `215` reply, cached.
    pub(crate) fn syst(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        if let Some(syst) = &self.syst {
            return Ok(Some(syst.clone()));
        }
        if !self.command(req, b"SYST", None, |c| c == Code::System)? {
            return Ok(None);
        }
        let text = c_str(&self.inbuf);
        let text = &text[text.iter().take_while(|&&b| b == b' ').count()..];
        let word = text
            .split(|&b| b == b' ')
            .next()
            .unwrap_or_default()
            .to_vec();
        self.syst = Some(word.clone());
        Ok(Some(word))
    }

    /// `ftp_pwd`: what the `257` reply quotes, cached until a `CWD`.
    pub(crate) fn pwd(&mut self, req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
        if let Some(pwd) = &self.pwd {
            return Ok(Some(pwd.clone()));
        }
        if !self.command(req, b"PWD", None, |c| c == Code::Path)? {
            return Ok(None);
        }
        let Some(pwd) = quoted(c_str(&self.inbuf)) else {
            return Ok(None);
        };
        self.pwd = Some(pwd.clone());
        Ok(Some(pwd))
    }

    /// `SITE EXEC`, `CWD`, `CDUP`, `RMD`, `DELE`, `SITE`: one command
    /// held to one code, or to any completion for `SITE`.
    pub(crate) fn simple(
        &mut self,
        req: &mut Request,
        verb: Verb,
        arg: &[u8],
    ) -> Result<bool, Thrown> {
        let (cmd, ok): (&[u8], fn(Code) -> bool) = match verb {
            Verb::Exec => (b"SITE EXEC", |c| c == Code::Okay),
            Verb::Chdir | Verb::Cdup => {
                self.pwd = None;
                if verb == Verb::Cdup {
                    return self.command(req, b"CDUP", None, |c| c == Code::FileDone);
                }
                (b"CWD", |c| c == Code::FileDone)
            }
            Verb::Rmdir => (b"RMD", |c| c == Code::FileDone),
            Verb::Delete => (b"DELE", |c| c == Code::FileDone),
            Verb::Site => (b"SITE", Code::completed),
        };
        self.command(req, cmd, Some(arg), ok)
    }

    /// `ftp_raw`: the command, then every line up to the one that ends
    /// the reply; `None` where the command was not sent.
    pub(crate) fn raw(
        &mut self,
        req: &mut Request,
        cmd: &[u8],
    ) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
        if !self.putcmd(req, cmd, None) {
            return Ok(None);
        }
        let mut lines = Vec::new();
        while self.readline(req)? {
            let line = c_str(&self.inbuf).to_vec();
            let last = is_end(&line);
            lines.push(line);
            if last {
                break;
            }
        }
        Ok(Some(lines))
    }

    /// `ftp_mkdir`: `MKD`, and the path the `257` reply quotes, the one
    /// given where it quotes none.
    pub(crate) fn mkdir(
        &mut self,
        req: &mut Request,
        dir: &[u8],
    ) -> Result<Option<Vec<u8>>, Thrown> {
        if !self.command(req, b"MKD", Some(dir), |c| c == Code::Path)? {
            return Ok(None);
        }
        let text = c_str(&self.inbuf);
        if !text.contains(&b'"') {
            return Ok(Some(dir.to_vec()));
        }
        Ok(quoted(text))
    }

    /// `ftp_chmod`: `SITE CHMOD <octal> <file>`.
    #[allow(clippy::cast_possible_truncation)]
    pub(crate) fn chmod(
        &mut self,
        req: &mut Request,
        mode: i64,
        file: &[u8],
    ) -> Result<bool, Thrown> {
        if file.is_empty() {
            return Ok(false);
        }
        let mut arg = format!("CHMOD {:o} ", (mode as i32).cast_unsigned()).into_bytes();
        arg.extend_from_slice(c_str(file));
        self.command(req, b"SITE", Some(&arg), |c| c == Code::Okay)
    }

    /// `ftp_alloc`: `ALLO`, and the reply's text where it was read.
    pub(crate) fn alloc(
        &mut self,
        req: &mut Request,
        size: i64,
    ) -> Result<(bool, Option<Vec<u8>>), Thrown> {
        if size <= 0 {
            return Ok((false, None));
        }
        if !self.putcmd(req, b"ALLO", Some(size.to_string().as_bytes())) || !self.getresp(req)? {
            return Ok((false, None));
        }
        let text = c_str(&self.inbuf).to_vec();
        Ok((self.resp.is_some_and(Code::completed), Some(text)))
    }

    /// `ftp_size`: `SIZE`'s number, `-1` where it fails.
    pub(crate) fn size(&mut self, req: &mut Request, path: &[u8]) -> Result<i64, Thrown> {
        if !self.set_type(req, Kind::Image)?
            || !self.command(req, b"SIZE", Some(path), |c| c == Code::FileStatus)?
        {
            return Ok(-1);
        }
        Ok(strtol(c_str(&self.inbuf)))
    }

    /// `ftp_mdtm`: `MDTM`'s `YYYYMMDDhhmmss`, read as UTC, `-1` where it
    /// fails.
    pub(crate) fn mdtm(&mut self, req: &mut Request, path: &[u8]) -> Result<i64, Thrown> {
        if !self.command(req, b"MDTM", Some(path), |c| c == Code::FileStatus)? {
            return Ok(-1);
        }
        Ok(mdtm_stamp(c_str(&self.inbuf)).unwrap_or(-1))
    }

    /// `ftp_rename`: `RNFR`, `350`, then `RNTO`, `250`.
    pub(crate) fn rename(
        &mut self,
        req: &mut Request,
        from: &[u8],
        to: &[u8],
    ) -> Result<bool, Thrown> {
        Ok(
            self.command(req, b"RNFR", Some(from), |c| c == Code::Pending)?
                && self.command(req, b"RNTO", Some(to), |c| c == Code::FileDone)?,
        )
    }

    /// `ftp_type`: `TYPE A` or `TYPE I` where it is not in force.
    fn set_type(&mut self, req: &mut Request, kind: Kind) -> Result<bool, Thrown> {
        if self.kind == Some(kind) {
            return Ok(true);
        }
        let letter: &[u8] = match kind {
            Kind::Ascii => b"A",
            Kind::Image => b"I",
        };
        if !self.command(req, b"TYPE", Some(letter), |c| c == Code::Okay)? {
            return Ok(false);
        }
        self.kind = Some(kind);
        Ok(true)
    }

    /// `ftp_pasv`: passive transfers off, or on with the address the server
    /// gives, `EPSV` first over IPv6, then `PASV`.
    pub(crate) fn pasv(&mut self, req: &mut Request, on: bool) -> Result<bool, Thrown> {
        if on && matches!(self.pasv, Pasv::Ready(_)) {
            return Ok(true);
        }
        self.pasv = Pasv::Off;
        if !on {
            return Ok(true);
        }
        let Some(peer) = self.control.socket_name(req, true)? else {
            return Ok(false);
        };
        let peer_host = host_of(peer.as_bytes());
        peer.give_up(req);
        if peer_host.contains(':') {
            if !self.putcmd(req, b"EPSV", None) || !self.getresp(req)? {
                return Ok(false);
            }
            if self.resp == Some(Code::ExtendedPassive) {
                let Some(port) = epsv_port(c_str(&self.inbuf)) else {
                    return Ok(false);
                };
                self.pasv =
                    Pasv::Ready(String::from_utf8_lossy(&transport(&peer_host, port)).into_owned());
                return Ok(true);
            }
        }
        if !self.command(req, b"PASV", None, |c| c == Code::Passive)? {
            return Ok(false);
        }
        let Some((ip, port)) = pasv_address(c_str(&self.inbuf)) else {
            return Ok(false);
        };
        let host = if self.usepasvaddress && !peer_host.contains(':') {
            ip
        } else {
            peer_host
        };
        self.pasv = Pasv::Ready(String::from_utf8_lossy(&transport(&host, port)).into_owned());
        Ok(true)
    }

    /// `ftp_putcmd`: `cmd args\r\n`, or `cmd\r\n` where there are no
    /// args, refused where either holds a line end or the line is longer
    /// than the buffer; the last reply's text and the unread bytes let
    /// go.
    fn putcmd(&mut self, req: &mut Request, cmd: &[u8], args: Option<&[u8]>) -> bool {
        let cmd = c_str(cmd);
        if cmd.iter().any(|&b| b == b'\r' || b == b'\n') {
            return false;
        }
        let mut line = cmd.to_vec();
        match args.map(c_str).filter(|a| !a.is_empty()) {
            Some(args) => {
                if cmd.len() + args.len() + 4 > BUFSIZE
                    || args.iter().any(|&b| b == b'\r' || b == b'\n')
                {
                    return false;
                }
                line.push(b' ');
                line.extend_from_slice(args);
            }
            None if cmd.len() + 3 > BUFSIZE => return false,
            None => {}
        }
        line.extend_from_slice(b"\r\n");
        self.inbuf.clear();
        self.extra.clear();
        send(req, *self.control, &line)
    }

    /// `ftp_readline`: the next line into `inbuf`, ended by `\r`, `\n` or
    /// `\r\n`, what follows kept for the next; `false` where the
    /// connection gave out first, `inbuf` holding what it gave.
    fn readline(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let mut buf = std::mem::take(&mut self.extra);
        loop {
            if let Some(end) = buf.iter().position(|&b| b == b'\r' || b == b'\n') {
                let mut rest = end + 1;
                if buf[end] == b'\r' && buf.get(rest) == Some(&b'\n') {
                    rest += 1;
                }
                self.extra = buf.split_off(rest);
                buf.truncate(end);
                self.inbuf = buf;
                return Ok(true);
            }
            if buf.len() + 2 > BUFSIZE {
                self.inbuf = buf;
                return Ok(false);
            }
            let room = BUFSIZE - 1 - buf.len();
            match recv(req, *self.control, room, seconds(self.timeout))? {
                Recv::Data(bytes) => buf.extend_from_slice(&bytes),
                Recv::Eof | Recv::Failed => {
                    self.inbuf = buf;
                    return Ok(false);
                }
            }
        }
    }

    /// `ftp_getresp`: lines until one opens with three digits and a
    /// blank, its code decoded and `inbuf` left at its text.
    fn getresp(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        self.resp = None;
        loop {
            if !self.readline(req)? {
                return Ok(false);
            }
            if is_end(c_str(&self.inbuf)) {
                break;
            }
        }
        let digits = &self.inbuf[..3];
        let n = digits
            .iter()
            .fold(0u16, |n, &d| n * 10 + u16::from(d - b'0'));
        self.resp = Some(Code::of(n));
        self.inbuf.drain(..4);
        Ok(true)
    }

    /// `ftp_getdata`: the data connection the next transfer runs over,
    /// connected to the passive address, or listening where the `PORT`
    /// (`EPRT` over IPv6) command tells the server to connect.
    fn getdata(&mut self, req: &mut Request) -> Result<Option<Data>, Thrown> {
        if self.pasv != Pasv::Off && !self.pasv(req, true)? {
            return Ok(None);
        }
        let context = self.context.as_deref().copied();
        if let Pasv::Ready(target) = &self.pasv {
            let target = target.clone().into_bytes();
            self.pasv = Pasv::On;
            let xport = Xport::Client {
                timeout: Some(seconds(self.timeout)),
                connect: true,
                persistent: None,
                socket: SocketOptions::default(),
                context,
            };
            return Ok(match req.xport_create(&target, xport)? {
                Ok(stream) => Some(Data::Connected(stream)),
                Err(XportFailure { code, text, .. }) => {
                    let text = text.unwrap_or_default();
                    let text = format!("php_connect_nonb() failed: {text} ({code})");
                    docref(req, Level::Warning, &text);
                    None
                }
            });
        }
        let v6 = self.local.contains(':');
        let any: &[u8] = if v6 {
            b"tcp://[::]:0"
        } else {
            b"tcp://0.0.0.0:0"
        };
        let xport = Xport::Server {
            listen: true,
            socket: SocketOptions {
                listen_queue: Some(5),
                ..SocketOptions::default()
            },
            context,
        };
        let listener = match req.xport_create(any, xport)? {
            Ok(stream) => stream,
            Err(XportFailure { code, text, .. }) => {
                let text = text.unwrap_or_default();
                docref(
                    req,
                    Level::Warning,
                    &format!("bind() failed: {text} ({code})"),
                );
                return Ok(None);
            }
        };
        let name = match listener.socket_name(req, false) {
            Ok(name) => name,
            Err(thrown) => {
                listener.free(req);
                return Err(thrown);
            }
        };
        let port = name.map_or(0, |name| {
            let bytes = name.as_bytes().to_vec();
            name.give_up(req);
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.rsplit_once(':')
                .and_then(|(_, p)| p.parse().ok())
                .unwrap_or(0u16)
        });
        let sent = if v6 {
            let arg = format!("|2|{}|{port}|", self.local);
            self.command(req, b"EPRT", Some(arg.as_bytes()), |c| c == Code::Okay)
        } else {
            let ip = self.local.replace('.', ",");
            let arg = format!("{ip},{},{}", port >> 8, port & 0xff);
            self.command(req, b"PORT", Some(arg.as_bytes()), |c| c == Code::Okay)
        };
        match sent {
            Ok(true) => Ok(Some(Data::Listening(listener))),
            sent => {
                listener.free(req);
                sent.map(|_| None)
            }
        }
    }

    /// `data_accept`: the connection the server makes to a listening
    /// socket within the timeout, then, for FTPS, its handshake over the
    /// control connection's session.
    fn accept(&mut self, req: &mut Request, data: Data) -> Result<Option<Owner<Stream>>, Thrown> {
        let stream = match data {
            Data::Connected(stream) => stream,
            Data::Listening(listener) => {
                let accepted = listener.accept(req, Some(seconds(self.timeout)), false);
                listener.free(req);
                let Ok((stream, peer)) = accepted? else {
                    timed_out(req);
                    return Ok(None);
                };
                if let Some(peer) = peer {
                    peer.give_up(req);
                }
                stream
            }
        };
        if matches!(self.tls, Tls::Up { data: true }) && !secure(req, *stream, Some(*self.control))
        {
            docref(req, Level::Warning, "data_accept: SSL/TLS handshake failed");
            stream.free(req);
            return Ok(None);
        }
        Ok(Some(stream))
    }

    /// The opening of a transfer: `TYPE`, the data connection, `REST`
    /// where `resume` is past zero, the command and its `150` or `125`,
    /// then the connection accepted.
    fn open_transfer(
        &mut self,
        req: &mut Request,
        kind: Kind,
        cmd: &[u8],
        path: &[u8],
        resume: i64,
    ) -> Result<Option<Owner<Stream>>, Thrown> {
        if !self.set_type(req, kind)? {
            return Ok(None);
        }
        let Some(data) = self.getdata(req)? else {
            return Ok(None);
        };
        match self.opened(req, cmd, path, resume) {
            Ok(true) => self.accept(req, data),
            opened => {
                close_data(req, data);
                opened.map(|_| None)
            }
        }
    }

    /// `REST` and the command of [`Self::open_transfer`].
    fn opened(
        &mut self,
        req: &mut Request,
        cmd: &[u8],
        path: &[u8],
        resume: i64,
    ) -> Result<bool, Thrown> {
        if resume > 0
            && !self.command(req, b"REST", Some(resume.to_string().as_bytes()), |c| {
                c == Code::Pending
            })?
        {
            return Ok(false);
        }
        self.command(req, cmd, Some(path), |c| {
            matches!(c, Code::Opening | Code::AlreadyOpen)
        })
    }

    /// The end of a transfer: its reply, `226` or `250`, and for a
    /// store `200` too.
    fn finished(&mut self, req: &mut Request, store: bool) -> Result<bool, Thrown> {
        Ok(self.getresp(req)?
            && self.resp.is_some_and(|c| {
                matches!(c, Code::Transferred | Code::FileDone) || (store && c == Code::Okay)
            }))
    }

    /// `ftp_get`: `RETR` into `out`, every `\r` dropped in ASCII.
    pub(crate) fn get(
        &mut self,
        req: &mut Request,
        out: Stream,
        path: &[u8],
        kind: Kind,
        resume: i64,
    ) -> Result<bool, Thrown> {
        let Some(data) = self.open_transfer(req, kind, b"RETR", path, resume)? else {
            return Ok(false);
        };
        let read = drain(req, *data, seconds(self.timeout), |req, bytes| {
            let bytes: Vec<u8> = match kind {
                Kind::Ascii => bytes.into_iter().filter(|&b| b != b'\r').collect(),
                Kind::Image => bytes,
            };
            let written = out.write(req, &bytes);
            kind != Kind::Image || written == Some(bytes.len())
        });
        data.free(req);
        if !read? {
            return Ok(false);
        }
        self.finished(req, false)
    }

    /// `ftp_put` and `ftp_append`: `STOR` or `APPE` of what `input` holds.
    pub(crate) fn put(
        &mut self,
        req: &mut Request,
        cmd: &[u8],
        path: &[u8],
        input: Stream,
        kind: Kind,
        start: i64,
    ) -> Result<bool, Thrown> {
        let Some(data) = self.open_transfer(req, kind, cmd, path, start)? else {
            return Ok(false);
        };
        let sent = send_stream(req, *data, input, kind, false);
        data.free(req);
        if !sent? {
            return Ok(false);
        }
        self.finished(req, true)
    }

    /// `ftp_genlist`: `NLST`, `LIST` or `MLSD` read whole over a data
    /// connection, cut at each `\r\n`; what follows the last is dropped.
    pub(crate) fn genlist(
        &mut self,
        req: &mut Request,
        cmd: &[u8],
        path: &[u8],
    ) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
        if !self.set_type(req, Kind::Ascii)? {
            return Ok(None);
        }
        let Some(data) = self.getdata(req)? else {
            return Ok(None);
        };
        let answered = self.command(req, cmd, Some(path), |c| {
            matches!(c, Code::Opening | Code::AlreadyOpen | Code::Transferred)
        });
        if !matches!(answered, Ok(true)) || self.resp == Some(Code::Transferred) {
            close_data(req, data);
            return Ok(answered?.then(Vec::new));
        }
        let Some(data) = self.accept(req, data)? else {
            return Ok(None);
        };
        let mut all = Vec::new();
        let read = drain(req, *data, seconds(self.timeout), |_, bytes| {
            all.extend_from_slice(&bytes);
            true
        });
        data.free(req);
        if !read? {
            return Ok(None);
        }
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut line = Vec::new();
        let mut last = 0u8;
        for &b in &all {
            if b == b'\n' && last == b'\r' {
                line.pop();
                lines.push(c_str(&line).to_vec());
                line.clear();
            } else {
                line.push(b);
            }
            last = b;
        }
        if !self.finished(req, false)? {
            return Ok(None);
        }
        Ok(Some(lines))
    }

    /// `ftp_nb_get`: a transfer in flight let go with its reply read
    /// first, then `RETR` opened and the first read done.
    pub(crate) fn nb_get(
        &mut self,
        req: &mut Request,
        out: Owner<Stream>,
        close: bool,
        path: &[u8],
        kind: Kind,
        resume: i64,
    ) -> Result<Progress, Thrown> {
        let opened = if self.nb.is_some() {
            self.abort(req);
            self.finished(req, false)
        } else {
            Ok(true)
        };
        let opened = match opened {
            Ok(true) => self.open_transfer(req, kind, b"RETR", path, resume),
            opened => opened.map(|_| None),
        };
        let data = match opened {
            Ok(Some(data)) => data,
            opened => {
                let_go(req, out, close);
                return opened.map(|_| Progress::Failed);
            }
        };
        self.nb = Some(Nb {
            data,
            stream: out,
            way: Way::Get { lastch: 0 },
            close,
        });
        self.nb_continue(req)
    }

    /// `ftp_nb_put`: `STOR` opened and the first write done.
    pub(crate) fn nb_put(
        &mut self,
        req: &mut Request,
        path: &[u8],
        input: Owner<Stream>,
        close: bool,
        kind: Kind,
        start: i64,
    ) -> Result<Progress, Thrown> {
        let data = match self.open_transfer(req, kind, b"STOR", path, start) {
            Ok(Some(data)) => data,
            opened => {
                let_go(req, input, close);
                return opened.map(|_| Progress::Failed);
            }
        };
        self.nb = Some(Nb {
            data,
            stream: input,
            way: Way::Put,
            close,
        });
        self.nb_continue(req)
    }

    /// `ftp_nb_continue_read` and `ftp_nb_continue_write`: one read or
    /// write of the transfer in flight, and its end where it is done. The
    /// streams stay the transfer's; the caller closes one it opened.
    pub(crate) fn nb_continue(&mut self, req: &mut Request) -> Result<Progress, Thrown> {
        let Some(nb) = self.nb.as_mut() else {
            return Ok(Progress::Failed);
        };
        let (data, stream, way) = (*nb.data, *nb.stream, nb.way);
        match way {
            Way::Get { lastch } => match recv(req, data, BUFSIZE, Duration::from_secs(1))? {
                Recv::Data(bytes) => {
                    let mut last = lastch;
                    let out = match self.kind {
                        Some(Kind::Ascii) => {
                            let mut out = Vec::with_capacity(bytes.len());
                            for &b in &bytes {
                                if last == b'\r' && b != b'\n' {
                                    out.push(b'\r');
                                }
                                if b != b'\r' {
                                    out.push(b);
                                }
                                last = b;
                            }
                            out
                        }
                        _ => bytes,
                    };
                    let written = stream.write(req, &out);
                    if self.kind != Some(Kind::Ascii) && written != Some(out.len()) {
                        return Ok(self.nb_failed(req));
                    }
                    if let Some(nb) = self.nb.as_mut() {
                        nb.way = Way::Get { lastch: last };
                    }
                    Ok(Progress::MoreData)
                }
                Recv::Failed if !data_gone(req, data)? => Ok(Progress::MoreData),
                Recv::Failed => Ok(self.nb_failed(req)),
                Recv::Eof => {
                    if self.kind == Some(Kind::Ascii) && lastch == b'\r' {
                        stream.write(req, b"\r");
                    }
                    self.nb_done(req)
                }
            },
            Way::Put => {
                let kind = self.kind.unwrap_or(Kind::Image);
                if !send_stream(req, data, stream, kind, true)? {
                    return Ok(self.nb_failed(req));
                }
                if !stream.eof(req) {
                    return Ok(Progress::MoreData);
                }
                self.nb_done(req)
            }
        }
    }

    /// The end of a non-blocking transfer that ran out: its data
    /// connection closed and its reply read.
    /// The transfer taken out of flight, php's `nb = 0`, its data
    /// connection closed, then its reply read and its stream let go, closed
    /// where the native opened it.
    fn nb_done(&mut self, req: &mut Request) -> Result<Progress, Thrown> {
        let nb = self.nb.take();
        let stream = nb.map(|nb| {
            nb.data.free(req);
            (nb.stream, nb.close)
        });
        let done = self.finished(req, false);
        if let Some((stream, close)) = stream {
            let_go(req, stream, close);
        }
        Ok(if done? {
            Progress::Finished
        } else {
            Progress::Failed
        })
    }

    /// A non-blocking transfer that failed, taken out of flight: its data
    /// connection closed and its stream let go.
    fn nb_failed(&mut self, req: &mut Request) -> Progress {
        self.abort(req);
        Progress::Failed
    }
}

/// The owner of a transfer's stream let go: closed where the native opened
/// it (`closestream`), else given up, the program's own name for it left.
fn let_go(req: &mut Request, stream: Owner<Stream>, close: bool) {
    if close {
        stream.free(req);
    } else {
        stream.give_up(req);
    }
}

/// A data connection read to its end, each read waiting up to `timeout`
/// and its bytes handed to `take`: `false` where a read failed or `take`
/// refused what it was handed.
fn drain(
    req: &mut Request,
    data: Stream,
    timeout: Duration,
    mut take: impl FnMut(&mut Request, Vec<u8>) -> bool,
) -> Result<bool, Thrown> {
    loop {
        match recv(req, data, BUFSIZE, timeout)? {
            Recv::Data(bytes) => {
                if !take(req, bytes) {
                    return Ok(false);
                }
            }
            Recv::Eof => return Ok(true),
            Recv::Failed => return Ok(false),
        }
    }
}

/// The verbs of [`Ftp::simple`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Exec,
    Chdir,
    Cdup,
    Rmdir,
    Delete,
    Site,
}

/// A line that ends a reply: three digits and a blank.
fn is_end(line: &[u8]) -> bool {
    line.len() >= 4 && line[..3].iter().all(u8::is_ascii_digit) && line[3] == b' '
}

/// What lies between the first `"` and the last one after it.
fn quoted(text: &[u8]) -> Option<Vec<u8>> {
    let open = text.iter().position(|&b| b == b'"')? + 1;
    let close = text[open..].iter().rposition(|&b| b == b'"')? + open;
    Some(text[open..close].to_vec())
}

/// `sscanf("%lu,%lu,%lu,%lu,%lu,%lu")` from the first digit of a `227`
/// reply: the address and the number to connect to, each cut to a byte.
#[allow(clippy::cast_possible_truncation)]
fn pasv_address(text: &[u8]) -> Option<(String, u16)> {
    let start = text.iter().position(u8::is_ascii_digit)?;
    let mut b = [0u8; 6];
    let mut rest = &text[start..];
    for (i, slot) in b.iter_mut().enumerate() {
        if i > 0 {
            rest = rest.strip_prefix(b",")?;
        }
        rest = rest.trim_ascii_start();
        let n = rest.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return None;
        }
        let value = rest[..n].iter().fold(0u64, |v, &d| {
            v.wrapping_mul(10).wrapping_add(u64::from(d - b'0'))
        });
        *slot = value as u8;
        rest = &rest[n..];
    }
    Some((
        format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
        u16::from_be_bytes([b[4], b[5]]),
    ))
}

/// The number to connect to of a `229` reply, `(|||n|)` with any delimiter.
fn epsv_port(text: &[u8]) -> Option<u16> {
    let open = text.iter().position(|&b| b == b'(')?;
    let delimiter = *text.get(open + 1)?;
    let mut at = open + 1;
    let mut seen = 0;
    while seen < 3 && at < text.len() {
        if text[at] == delimiter {
            seen += 1;
        }
        at += 1;
    }
    let digits = text[at..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || text.get(at + digits) != Some(&delimiter) {
        return None;
    }
    let n = text[at..at + digits].iter().fold(0u64, |v, &d| {
        v.wrapping_mul(10).wrapping_add(u64::from(d - b'0'))
    });
    Some(port16(i64::try_from(n).unwrap_or(0)))
}

/// `MDTM`'s six numbers from the first digit, read as UTC, which is
/// what `ftp_mdtm`'s `mktime` of the time moved by the offset of now
/// comes to.
fn mdtm_stamp(text: &[u8]) -> Option<i64> {
    let start = text.iter().position(u8::is_ascii_digit)?;
    let mut rest = &text[start..];
    let mut fields = [0i64; 6];
    for (slot, width) in fields.iter_mut().zip([4usize, 2, 2, 2, 2, 2]) {
        let n = rest
            .iter()
            .take(width)
            .take_while(|b| b.is_ascii_digit())
            .count();
        if n == 0 {
            return None;
        }
        *slot = rest[..n]
            .iter()
            .fold(0, |v, &d| v * 10 + i64::from(d - b'0'));
        rest = &rest[n..];
    }
    let [year, month, day, hour, minute, second] = fields;
    let year = year + (month - 1).div_euclid(12);
    let month = (month - 1).rem_euclid(12) + 1;
    let days = days_from_civil(year, month, 1) + day - 1;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `php_stream_xport_crypto_setup` and `php_stream_xport_crypto_enable` over `SSLv23`, with the
/// session of `session` where it is given; whether it came up.
fn secure(req: &mut Request, stream: Stream, session: Option<Stream>) -> bool {
    stream.crypto_setup(req, SSLV23_CLIENT, session)
        && matches!(stream.crypto_enable(req, true), Handshake::Done)
}

/// `data_close` of a connection not yet accepted.
fn close_data(req: &mut Request, data: Data) {
    match data {
        Data::Listening(stream) | Data::Connected(stream) => stream.free(req),
    }
}

/// Whether a data connection that gave nothing within the second is
/// gone rather than slow: at its end.
fn data_gone(req: &mut Request, data: Stream) -> Result<bool, Thrown> {
    Ok(!ran_out(req, data)?)
}

/// `ftp_send_stream_to_data_socket`: what `input` holds sent, in ASCII a
/// line at a time with each `\n` sent as `\r\n`, buffered to
/// `FTP_BUFSIZE`; `once` sends one buffer and stops, as a non-blocking
/// transfer does.
fn send_stream(
    req: &mut Request,
    data: Stream,
    input: Stream,
    kind: Kind,
    once: bool,
) -> Result<bool, Thrown> {
    match kind {
        Kind::Ascii => {
            let mut buf: Vec<u8> = Vec::with_capacity(BUFSIZE);
            while !input.eof(req) {
                let room = BUFSIZE - buf.len();
                let Some(line) = input.get_line(req, Some(room))? else {
                    break;
                };
                buf.extend_from_slice(&line);
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                    buf.extend_from_slice(b"\r\n");
                }
                if BUFSIZE - buf.len() < 2 {
                    if !send(req, data, &buf) {
                        return Ok(false);
                    }
                    buf.clear();
                    if once {
                        break;
                    }
                }
            }
            Ok(buf.is_empty() || send(req, data, &buf))
        }
        Kind::Image => {
            while !input.eof(req) {
                let Some(chunk) = input.read(req, BUFSIZE)? else {
                    return Ok(false);
                };
                let bytes = chunk.as_bytes().to_vec();
                chunk.give_up(req);
                if bytes.is_empty() {
                    break;
                }
                if !send(req, data, &bytes) {
                    return Ok(false);
                }
                if once {
                    break;
                }
            }
            Ok(true)
        }
    }
}

/// `ftp_mlsd_parse_line`: the entry's `name`, then its facts, each
/// `key=value;` before the blank; the warning and `None` for a line that
/// breaks the form.
pub(crate) fn mlsd_line(req: &mut Request, line: &[u8]) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let Some(blank) = line.iter().position(|&b| b == b' ') else {
        docref(req, Level::Warning, "Missing pathname in MLSD response");
        return Ok(None);
    };
    let mut entry = PhpArray::new(req)?;
    let name = PhpString::new(req, &line[blank + 1..])?;
    entry.set_str(req, b"name", name.into_value())?;
    let mut facts = &line[..blank];
    while !facts.is_empty() {
        let fact = facts.iter().position(|&b| b == b';').and_then(|semi| {
            let eq = facts[..semi].iter().position(|&b| b == b'=')?;
            Some((eq, semi))
        });
        let Some((eq, semi)) = fact else {
            docref(req, Level::Warning, "Malformed fact in MLSD response");
            entry.give_up(req);
            return Ok(None);
        };
        let value = PhpString::new(req, &facts[eq + 1..semi])?;
        entry.set_str(req, &facts[..eq], value.into_value())?;
        facts = &facts[semi + 1..];
    }
    Ok(Some(entry))
}

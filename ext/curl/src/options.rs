//! `curl_setopt` and `curl_setopt_array` over one typed table: each
//! `CURLOPT_*` `_php_curl_setopt` accepts decoded once to the kind of
//! value it takes ([`Opt`]), and each kind set as php sets it.

use std::ops::ControlFlow;
use std::os::fd::RawFd;

use php_ext::abi::Instance;
use php_ext::attr::php_function;
use php_ext::coerce::{callable_checked, to_long, truthy, with_string};
use php_ext::raising::{Level, Thrown, argument_error, docref, type_error, value_error};
use php_ext::request::{Cast, Request, Stream};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, Value, ValueRef};

use crate::constants as c;
use crate::ffi::{Called, EasyId, Hook, Slist};
use crate::handle::{Curl, CurlHandle, Handler, HeaderTo, ReadFrom, WriteTo, easy_of};
use crate::throwables::{Pending, Throwables};

/// The three transfer channels a callable can take over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Chan {
    Write,
    Header,
    Read,
}

/// The four options that take a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamOpt {
    File,
    Infile,
    Stderr,
    WriteHeader,
}

/// What kind of value one option takes, as `_php_curl_setopt`'s
/// `switch` groups them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Opt {
    /// `HANDLE_CURL_OPTION_CALLABLE_PHP_CURL_USER`.
    Channel(Chan),
    /// `HANDLE_CURL_OPTION_CALLABLE`.
    Hook(Hook),
    /// A `long`, `zval_get_long`.
    Long,
    /// `CURLOPT_SSL_VERIFYHOST`, which no longer takes 1.
    VerifyHost,
    /// `CURLOPT_PROTOCOLS` and `CURLOPT_REDIR_PROTOCOLS`, which
    /// `open_basedir` keeps `file` out of.
    Protocols,
    /// `CURLOPT_SAFE_UPLOAD`, which only `true` is left for.
    SafeUpload,
    /// A string the library copies.
    Str,
    /// `CURLOPT_PROTOCOLS_STR` and `CURLOPT_REDIR_PROTOCOLS_STR`.
    ProtocolsStr,
    /// A string, or `null` to unset.
    NullableStr,
    Private,
    Url,
    Stream(StreamOpt),
    /// A list, `curl_slist`.
    List,
    /// `CURLOPT_BINARYTRANSFER` and `CURLOPT_DNS_USE_GLOBAL_CACHE`.
    Ignored,
    PostFields,
    ReturnTransfer,
    /// A `curl_off_t`.
    OffT,
    PostRedir,
    /// A path `open_basedir` holds.
    Path,
    /// `CURLINFO_HEADER_OUT`.
    HeaderOut,
    Share,
    /// A blob, copied.
    Blob,
}

/// The kind of value `option` takes, `None` for no option php knows.
#[allow(clippy::too_many_lines)]
pub(crate) fn decode(option: i64) -> Option<Opt> {
    Some(match option {
        c::CURLOPT_WRITEFUNCTION => Opt::Channel(Chan::Write),
        c::CURLOPT_HEADERFUNCTION => Opt::Channel(Chan::Header),
        c::CURLOPT_READFUNCTION => Opt::Channel(Chan::Read),
        c::CURLOPT_PROGRESSFUNCTION => Opt::Hook(Hook::Progress),
        c::CURLOPT_XFERINFOFUNCTION => Opt::Hook(Hook::Xferinfo),
        c::CURLOPT_FNMATCH_FUNCTION => Opt::Hook(Hook::Fnmatch),
        c::CURLOPT_DEBUGFUNCTION => Opt::Hook(Hook::Debug),
        c::CURLOPT_PREREQFUNCTION => Opt::Hook(Hook::Prereq),
        c::CURLOPT_SSH_HOSTKEYFUNCTION => Opt::Hook(Hook::SshHostkey),
        c::CURLOPT_SSL_VERIFYHOST => Opt::VerifyHost,
        c::CURLOPT_PROTOCOLS | c::CURLOPT_REDIR_PROTOCOLS => Opt::Protocols,
        c::CURLOPT_PROTOCOLS_STR | c::CURLOPT_REDIR_PROTOCOLS_STR => Opt::ProtocolsStr,
        c::CURLOPT_SAFE_UPLOAD => Opt::SafeUpload,
        c::CURLOPT_PRIVATE => Opt::Private,
        c::CURLOPT_URL => Opt::Url,
        c::CURLOPT_FILE => Opt::Stream(StreamOpt::File),
        c::CURLOPT_INFILE => Opt::Stream(StreamOpt::Infile),
        c::CURLOPT_STDERR => Opt::Stream(StreamOpt::Stderr),
        c::CURLOPT_WRITEHEADER => Opt::Stream(StreamOpt::WriteHeader),
        c::CURLOPT_HTTP200ALIASES
        | c::CURLOPT_HTTPHEADER
        | c::CURLOPT_POSTQUOTE
        | c::CURLOPT_PREQUOTE
        | c::CURLOPT_QUOTE
        | c::CURLOPT_TELNETOPTIONS
        | c::CURLOPT_MAIL_RCPT
        | c::CURLOPT_RESOLVE
        | c::CURLOPT_PROXYHEADER
        | c::CURLOPT_CONNECT_TO => Opt::List,
        c::CURLOPT_POSTFIELDS => Opt::PostFields,
        c::CURLOPT_RETURNTRANSFER => Opt::ReturnTransfer,
        c::CURLOPT_POSTREDIR => Opt::PostRedir,
        c::CURLINFO_HEADER_OUT => Opt::HeaderOut,
        c::CURLOPT_SHARE => Opt::Share,
        c::CURLOPT_AUTOREFERER
        | c::CURLOPT_BUFFERSIZE
        | c::CURLOPT_CONNECTTIMEOUT
        | c::CURLOPT_COOKIESESSION
        | c::CURLOPT_CRLF
        | c::CURLOPT_DNS_CACHE_TIMEOUT
        | c::CURLOPT_FAILONERROR
        | c::CURLOPT_FILETIME
        | c::CURLOPT_FORBID_REUSE
        | c::CURLOPT_FRESH_CONNECT
        | c::CURLOPT_FTP_USE_EPRT
        | c::CURLOPT_FTP_USE_EPSV
        | c::CURLOPT_HEADER
        | c::CURLOPT_HTTPGET
        | c::CURLOPT_HTTPPROXYTUNNEL
        | c::CURLOPT_HTTP_VERSION
        | c::CURLOPT_INFILESIZE
        | c::CURLOPT_LOW_SPEED_LIMIT
        | c::CURLOPT_LOW_SPEED_TIME
        | c::CURLOPT_MAXCONNECTS
        | c::CURLOPT_MAXREDIRS
        | c::CURLOPT_NETRC
        | c::CURLOPT_NOBODY
        | c::CURLOPT_NOPROGRESS
        | c::CURLOPT_NOSIGNAL
        | c::CURLOPT_PORT
        | c::CURLOPT_POST
        | c::CURLOPT_PROXYPORT
        | c::CURLOPT_PROXYTYPE
        | c::CURLOPT_PUT
        | c::CURLOPT_RESUME_FROM
        | c::CURLOPT_SSLVERSION
        | c::CURLOPT_SSL_VERIFYPEER
        | c::CURLOPT_TIMECONDITION
        | c::CURLOPT_TIMEOUT
        | c::CURLOPT_TIMEVALUE
        | c::CURLOPT_TRANSFERTEXT
        | c::CURLOPT_UNRESTRICTED_AUTH
        | c::CURLOPT_UPLOAD
        | c::CURLOPT_VERBOSE
        | c::CURLOPT_HTTPAUTH
        | c::CURLOPT_FTP_CREATE_MISSING_DIRS
        | c::CURLOPT_PROXYAUTH
        | c::CURLOPT_SERVER_RESPONSE_TIMEOUT
        | c::CURLOPT_IPRESOLVE
        | c::CURLOPT_MAXFILESIZE
        | c::CURLOPT_TCP_NODELAY
        | c::CURLOPT_FTPSSLAUTH
        | c::CURLOPT_IGNORE_CONTENT_LENGTH
        | c::CURLOPT_FTP_SKIP_PASV_IP
        | c::CURLOPT_FTP_FILEMETHOD
        | c::CURLOPT_CONNECT_ONLY
        | c::CURLOPT_LOCALPORT
        | c::CURLOPT_LOCALPORTRANGE
        | c::CURLOPT_SSL_SESSIONID_CACHE
        | c::CURLOPT_FTP_SSL_CCC
        | c::CURLOPT_SSH_AUTH_TYPES
        | c::CURLOPT_CONNECTTIMEOUT_MS
        | c::CURLOPT_HTTP_CONTENT_DECODING
        | c::CURLOPT_HTTP_TRANSFER_DECODING
        | c::CURLOPT_TIMEOUT_MS
        | c::CURLOPT_NEW_DIRECTORY_PERMS
        | c::CURLOPT_NEW_FILE_PERMS
        | c::CURLOPT_USE_SSL
        | c::CURLOPT_APPEND
        | c::CURLOPT_DIRLISTONLY
        | c::CURLOPT_PROXY_TRANSFER_MODE
        | c::CURLOPT_ADDRESS_SCOPE
        | c::CURLOPT_CERTINFO
        | c::CURLOPT_SOCKS5_GSSAPI_NEC
        | c::CURLOPT_TFTP_BLKSIZE
        | c::CURLOPT_FTP_USE_PRET
        | c::CURLOPT_RTSP_CLIENT_CSEQ
        | c::CURLOPT_RTSP_REQUEST
        | c::CURLOPT_RTSP_SERVER_CSEQ
        | c::CURLOPT_WILDCARDMATCH
        | c::CURLOPT_GSSAPI_DELEGATION
        | c::CURLOPT_ACCEPTTIMEOUT_MS
        | c::CURLOPT_SSL_OPTIONS
        | c::CURLOPT_TCP_KEEPALIVE
        | c::CURLOPT_TCP_KEEPIDLE
        | c::CURLOPT_TCP_KEEPINTVL
        | c::CURLOPT_SASL_IR
        | c::CURLOPT_EXPECT_100_TIMEOUT_MS
        | c::CURLOPT_SSL_ENABLE_ALPN
        | c::CURLOPT_SSL_ENABLE_NPN
        | c::CURLOPT_HEADEROPT
        | c::CURLOPT_SSL_VERIFYSTATUS
        | c::CURLOPT_PATH_AS_IS
        | c::CURLOPT_SSL_FALSESTART
        | c::CURLOPT_PIPEWAIT
        | c::CURLOPT_STREAM_WEIGHT
        | c::CURLOPT_TFTP_NO_OPTIONS
        | c::CURLOPT_TCP_FASTOPEN
        | c::CURLOPT_KEEP_SENDING_ON_ERROR
        | c::CURLOPT_PROXY_SSL_OPTIONS
        | c::CURLOPT_PROXY_SSL_VERIFYHOST
        | c::CURLOPT_PROXY_SSL_VERIFYPEER
        | c::CURLOPT_PROXY_SSLVERSION
        | c::CURLOPT_SUPPRESS_CONNECT_HEADERS
        | c::CURLOPT_SOCKS5_AUTH
        | c::CURLOPT_SSH_COMPRESSION
        | c::CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS
        | c::CURLOPT_DNS_SHUFFLE_ADDRESSES
        | c::CURLOPT_HAPROXYPROTOCOL
        | c::CURLOPT_DISALLOW_USERNAME_IN_URL
        | c::CURLOPT_UPKEEP_INTERVAL_MS
        | c::CURLOPT_UPLOAD_BUFFERSIZE
        | c::CURLOPT_HTTP09_ALLOWED
        | c::CURLOPT_ALTSVC_CTRL
        | c::CURLOPT_MAXAGE_CONN
        | c::CURLOPT_MAIL_RCPT_ALLLOWFAILS
        | c::CURLOPT_HSTS_CTRL
        | c::CURLOPT_DOH_SSL_VERIFYHOST
        | c::CURLOPT_DOH_SSL_VERIFYPEER
        | c::CURLOPT_DOH_SSL_VERIFYSTATUS
        | c::CURLOPT_MAXLIFETIME_CONN
        | c::CURLOPT_MIME_OPTIONS
        | c::CURLOPT_WS_OPTIONS
        | c::CURLOPT_CA_CACHE_TIMEOUT
        | c::CURLOPT_QUICK_EXIT
        | c::CURLOPT_TCP_KEEPCNT
        | c::CURLOPT_FOLLOWLOCATION => Opt::Long,
        c::CURLOPT_CAINFO
        | c::CURLOPT_CAPATH
        | c::CURLOPT_COOKIE
        | c::CURLOPT_EGDSOCKET
        | c::CURLOPT_INTERFACE
        | c::CURLOPT_PROXY
        | c::CURLOPT_PROXYUSERPWD
        | c::CURLOPT_REFERER
        | c::CURLOPT_SSLCERTTYPE
        | c::CURLOPT_SSLENGINE
        | c::CURLOPT_SSLENGINE_DEFAULT
        | c::CURLOPT_SSLKEY
        | c::CURLOPT_SSLKEYPASSWD
        | c::CURLOPT_SSLKEYTYPE
        | c::CURLOPT_SSL_CIPHER_LIST
        | c::CURLOPT_USERAGENT
        | c::CURLOPT_COOKIELIST
        | c::CURLOPT_FTP_ALTERNATIVE_TO_USER
        | c::CURLOPT_SSH_HOST_PUBLIC_KEY_MD5
        | c::CURLOPT_PROXYPASSWORD
        | c::CURLOPT_PROXYUSERNAME
        | c::CURLOPT_NOPROXY
        | c::CURLOPT_SOCKS5_GSSAPI_SERVICE
        | c::CURLOPT_MAIL_FROM
        | c::CURLOPT_RTSP_STREAM_URI
        | c::CURLOPT_RTSP_TRANSPORT
        | c::CURLOPT_TLSAUTH_TYPE
        | c::CURLOPT_TLSAUTH_PASSWORD
        | c::CURLOPT_TLSAUTH_USERNAME
        | c::CURLOPT_TRANSFER_ENCODING
        | c::CURLOPT_DNS_SERVERS
        | c::CURLOPT_MAIL_AUTH
        | c::CURLOPT_LOGIN_OPTIONS
        | c::CURLOPT_PINNEDPUBLICKEY
        | c::CURLOPT_PROXY_SERVICE_NAME
        | c::CURLOPT_SERVICE_NAME
        | c::CURLOPT_DEFAULT_PROTOCOL
        | c::CURLOPT_PRE_PROXY
        | c::CURLOPT_PROXY_CAINFO
        | c::CURLOPT_PROXY_CAPATH
        | c::CURLOPT_PROXY_CRLFILE
        | c::CURLOPT_PROXY_KEYPASSWD
        | c::CURLOPT_PROXY_PINNEDPUBLICKEY
        | c::CURLOPT_PROXY_SSL_CIPHER_LIST
        | c::CURLOPT_PROXY_SSLCERT
        | c::CURLOPT_PROXY_SSLCERTTYPE
        | c::CURLOPT_PROXY_SSLKEY
        | c::CURLOPT_PROXY_SSLKEYTYPE
        | c::CURLOPT_PROXY_TLSAUTH_PASSWORD
        | c::CURLOPT_PROXY_TLSAUTH_TYPE
        | c::CURLOPT_PROXY_TLSAUTH_USERNAME
        | c::CURLOPT_ABSTRACT_UNIX_SOCKET
        | c::CURLOPT_REQUEST_TARGET
        | c::CURLOPT_PROXY_TLS13_CIPHERS
        | c::CURLOPT_TLS13_CIPHERS
        | c::CURLOPT_ALTSVC
        | c::CURLOPT_SASL_AUTHZID
        | c::CURLOPT_PROXY_ISSUERCERT
        | c::CURLOPT_SSL_EC_CURVES
        | c::CURLOPT_AWS_SIGV4
        | c::CURLOPT_SSH_HOST_PUBLIC_KEY_SHA256 => Opt::Str,
        c::CURLOPT_CUSTOMREQUEST
        | c::CURLOPT_FTPPORT
        | c::CURLOPT_RANGE
        | c::CURLOPT_FTP_ACCOUNT
        | c::CURLOPT_RTSP_SESSION_ID
        | c::CURLOPT_ACCEPT_ENCODING
        | c::CURLOPT_DNS_INTERFACE
        | c::CURLOPT_DNS_LOCAL_IP4
        | c::CURLOPT_DNS_LOCAL_IP6
        | c::CURLOPT_XOAUTH2_BEARER
        | c::CURLOPT_UNIX_SOCKET_PATH
        | c::CURLOPT_DOH_URL
        | c::CURLOPT_HSTS
        | c::CURLOPT_KRBLEVEL
        | c::CURLOPT_USERPWD
        | c::CURLOPT_USERNAME
        | c::CURLOPT_PASSWORD
        | c::CURLOPT_SSL_SIGNATURE_ALGORITHMS => Opt::NullableStr,
        c::CURLOPT_BINARYTRANSFER | c::CURLOPT_DNS_USE_GLOBAL_CACHE => Opt::Ignored,
        c::CURLOPT_INFILESIZE_LARGE
        | c::CURLOPT_MAX_RECV_SPEED_LARGE
        | c::CURLOPT_MAX_SEND_SPEED_LARGE
        | c::CURLOPT_MAXFILESIZE_LARGE
        | c::CURLOPT_TIMEVALUE_LARGE => Opt::OffT,
        c::CURLOPT_COOKIEFILE
        | c::CURLOPT_COOKIEJAR
        | c::CURLOPT_RANDOM_FILE
        | c::CURLOPT_SSLCERT
        | c::CURLOPT_NETRC_FILE
        | c::CURLOPT_SSH_PRIVATE_KEYFILE
        | c::CURLOPT_SSH_PUBLIC_KEYFILE
        | c::CURLOPT_CRLFILE
        | c::CURLOPT_ISSUERCERT
        | c::CURLOPT_SSH_KNOWNHOSTS => Opt::Path,
        c::CURLOPT_ISSUERCERT_BLOB
        | c::CURLOPT_PROXY_ISSUERCERT_BLOB
        | c::CURLOPT_PROXY_SSLCERT_BLOB
        | c::CURLOPT_PROXY_SSLKEY_BLOB
        | c::CURLOPT_SSLCERT_BLOB
        | c::CURLOPT_SSLKEY_BLOB
        | c::CURLOPT_CAINFO_BLOB
        | c::CURLOPT_PROXY_CAINFO_BLOB => Opt::Blob,
        _ => return None,
    })
}

/// The name `zend_type_error` gives a list option that was not handed an
/// array.
const fn list_name(option: i64) -> &'static str {
    match option {
        c::CURLOPT_HTTPHEADER => "CURLOPT_HTTPHEADER",
        c::CURLOPT_QUOTE => "CURLOPT_QUOTE",
        c::CURLOPT_HTTP200ALIASES => "CURLOPT_HTTP200ALIASES",
        c::CURLOPT_POSTQUOTE => "CURLOPT_POSTQUOTE",
        c::CURLOPT_PREQUOTE => "CURLOPT_PREQUOTE",
        c::CURLOPT_TELNETOPTIONS => "CURLOPT_TELNETOPTIONS",
        c::CURLOPT_MAIL_RCPT => "CURLOPT_MAIL_RCPT",
        c::CURLOPT_RESOLVE => "CURLOPT_RESOLVE",
        c::CURLOPT_PROXYHEADER => "CURLOPT_PROXYHEADER",
        _ => "CURLOPT_CONNECT_TO",
    }
}

/// The name a callable option's refusal names.
const fn callable_name(option: i64) -> &'static str {
    match option {
        c::CURLOPT_WRITEFUNCTION => "CURLOPT_WRITEFUNCTION",
        c::CURLOPT_HEADERFUNCTION => "CURLOPT_HEADERFUNCTION",
        c::CURLOPT_READFUNCTION => "CURLOPT_READFUNCTION",
        c::CURLOPT_PROGRESSFUNCTION => "CURLOPT_PROGRESSFUNCTION",
        c::CURLOPT_XFERINFOFUNCTION => "CURLOPT_XFERINFOFUNCTION",
        c::CURLOPT_FNMATCH_FUNCTION => "CURLOPT_FNMATCH_FUNCTION",
        c::CURLOPT_DEBUGFUNCTION => "CURLOPT_DEBUGFUNCTION",
        c::CURLOPT_PREREQFUNCTION => "CURLOPT_PREREQFUNCTION",
        _ => "CURLOPT_SSH_HOSTKEYFUNCTION",
    }
}

/// What one option set answers: php's `SUCCESS` or `FAILURE`, a
/// throwable being the `Err`.
type Set = Result<bool, Thrown>;

/// Whether `open_basedir` is set, `PG(open_basedir) && *PG(open_basedir)`.
fn basedir(req: &Request) -> bool {
    req.ini()
        .get(b"open_basedir")
        .is_some_and(|value| !value.is_empty())
}

/// The `CurlHandle`'s payload.
fn payload(req: &mut Request, object: PhpObject) -> Result<&mut Curl, Thrown> {
    object.native_mut_or_fatal::<Curl>(req)
}

/// `SAVE_CURL_ERROR(ch, error)`, and whether it was `CURLE_OK`.
fn saved(req: &mut Request, object: PhpObject, code: i32) -> Set {
    payload(req, object)?.err = i64::from(code);
    Ok(code == 0)
}

/// A set the library answered, [`saved`]: what a callback threw during
/// it left in `pending`, as `zend_call_known_fcc` leaves it, the set
/// going on past it.
fn set(req: &mut Request, object: PhpObject, called: Called, pending: &mut Pending) -> Set {
    let (code, thrown) = called;
    if let Some(thrown) = thrown {
        pending.park(req, thrown);
    }
    saved(req, object, code)
}

/// `php_curl_option_str`: php's `ValueError` for a NUL, then the string
/// set, its code saved.
fn option_str(
    req: &mut Request,
    object: PhpObject,
    option: i64,
    value: &[u8],
    pending: &mut Pending,
) -> Set {
    if value.contains(&0) {
        let text = format!(
            "{}(): cURL option must not contain any null bytes",
            req.frames().active_function_name()
        );
        return Err(value_error(req, &text));
    }
    let easy = easy_of(req, object)?;
    let called = easy.set_str(req, option, Some(value));
    set(req, object, called, pending)
}

/// `php_curl_option_url`: `file://` kept out under `open_basedir`, then
/// the URL set.
pub(crate) fn set_url(
    req: &mut Request,
    object: PhpObject,
    url: &[u8],
    pending: &mut Pending,
) -> Set {
    if basedir(req) {
        let easy = easy_of(req, object)?;
        let (_, thrown) = easy.set_long(
            req,
            c::CURLOPT_PROTOCOLS,
            c::CURLPROTO_ALL & !c::CURLPROTO_FILE,
        );
        if let Some(thrown) = thrown {
            pending.park(req, thrown);
        }
    }
    option_str(req, object, c::CURLOPT_URL, url, pending)
}

/// `php_curl_set_callable_handler`: the callable a value names, `None`
/// for `null`; php's `TypeError` for anything else.
fn callable(
    req: &mut Request,
    value: &Value,
    option: i64,
    in_array: bool,
) -> Result<Option<Handler>, Thrown> {
    let value = value.deref();
    if matches!(value.as_ref(), ValueRef::Null) {
        return Ok(None);
    }
    match callable_checked(req, &value) {
        Ok(callable) => Ok(Some(Owner::take(req, callable))),
        Err(refusal) => {
            php_ext::raising::pending(req)?;
            let position = if in_array { 2 } else { 3 };
            let tail = format!(
                "must be a valid callback for option {}, {refusal}",
                callable_name(option)
            );
            let ce = Throwables::of(req)?;
            Err(argument_error(req, ce.type_error, position, &tail))
        }
    }
}

/// One channel's callable replaced: `None` for `null`, which puts the
/// channel back to its default.
fn set_channel(
    req: &mut Request,
    object: PhpObject,
    chan: Chan,
    value: &Value,
    option: i64,
    in_array: bool,
) -> Set {
    let old = {
        let curl = payload(req, object)?;
        match chan {
            Chan::Write => curl.write.handler.take(),
            Chan::Header => curl.header.handler.take(),
            Chan::Read => curl.read.handler.take(),
        }
    };
    if let Some(old) = old {
        old.give_up(req);
    }
    let handler = callable(req, value, option, in_array);
    let curl = payload(req, object)?;
    let user = matches!(handler, Ok(Some(_)));
    match chan {
        Chan::Write => curl.write.to = if user { WriteTo::User } else { WriteTo::Stdout },
        Chan::Header => {
            curl.header.to = if user {
                HeaderTo::User
            } else {
                HeaderTo::Ignore
            }
        }
        Chan::Read => {
            curl.read.to = if user {
                ReadFrom::User
            } else {
                ReadFrom::Direct
            }
        }
    }
    match handler? {
        None => Ok(true),
        Some(handler) => {
            match chan {
                Chan::Write => curl.write.handler = Some(handler),
                Chan::Header => curl.header.handler = Some(handler),
                Chan::Read => curl.read.handler = Some(handler),
            }
            saved(req, object, 0)
        }
    }
}

/// One hook's callable replaced and its trampoline installed.
fn set_hook(
    req: &mut Request,
    object: PhpObject,
    hook: Hook,
    value: &Value,
    option: i64,
    in_array: bool,
    pending: &mut Pending,
) -> Set {
    if let Some(old) = payload(req, object)?.hook_mut(hook).take() {
        old.give_up(req);
    }
    let handler = callable(req, value, option, in_array)?;
    *payload(req, object)?.hook_mut(hook) = handler;
    if let Some(thrown) = easy_of(req, object)?.set_hook(req, hook) {
        pending.park(req, thrown);
    }
    saved(req, object, 0)
}

/// The stream a stream option names, as `zend_fetch_resource2_ex`
/// fetches it and `php_stream_cast(PHP_STREAM_AS_STDIO)` casts it, which
/// never fails where php has `fopencookie` or `funopen`: the stream and
/// the fd php `fdopen`s, none where its `FILE *` is a cookie over the
/// stream, or php's `TypeError`.
///
/// ```php
/// foreach (['php://temp', 'php://memory'] as $url) {
///     $h = curl_init('file:///i_dont_exist/');
///     curl_setopt($h, CURLOPT_VERBOSE, true);
///     curl_setopt($h, CURLOPT_RETURNTRANSFER, true);
///     var_dump(curl_setopt($h, CURLOPT_STDERR, $o = fopen($url, 'w+')));
///     curl_exec($h);
///     rewind($o);
///     var_dump(stream_get_contents($o));
/// }
/// $c = curl_copy_handle($h);
/// unset($h);
/// curl_exec($c);
/// rewind($o);
/// var_dump(stream_get_contents($o));
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "body\n");
/// $h = curl_init("file://$f");
/// var_dump(curl_setopt($h, CURLOPT_FILE, $m = fopen('php://memory', 'w+')), curl_exec($h));
/// rewind($m);
/// var_dump(stream_get_contents($m));
/// unlink($f);
/// ```
/// ```expect
/// bool(true)
/// string(37) "* Could not open file /i_dont_exist/
/// "
/// bool(true)
/// string(37) "* Could not open file /i_dont_exist/
/// "
/// string(74) "* Could not open file /i_dont_exist/
/// * Could not open file /i_dont_exist/
/// "
/// bool(true)
/// bool(true)
/// string(5) "body
/// "
/// ```
fn stream_of(req: &mut Request, value: &Value) -> Result<(Stream, Option<RawFd>), Thrown> {
    let Some(resource) = value.as_resource() else {
        let text = format!(
            "{}(): supplied argument is not a valid File-Handle resource",
            req.frames().active_function_name()
        );
        return Err(type_error(req, &text));
    };
    let Some(stream) = Stream::from_value(req, &resource.as_value()) else {
        let text = format!(
            "{}(): supplied resource is not a valid File-Handle resource",
            req.frames().active_function_name()
        );
        return Err(type_error(req, &text));
    };
    Ok((stream, stream.cast(req, Cast::Stdio, true)))
}

/// `what->mode[0] != 'r' || what->mode[1] == '+'`.
fn writable(req: &mut Request, stream: Stream) -> Result<bool, Thrown> {
    let meta = stream.meta_data(req)?;
    let mode = meta
        .find(b"mode")
        .and_then(|mode| mode.deref().as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default();
    meta.give_up(req);
    Ok(mode.first() != Some(&b'r') || mode.get(1) == Some(&b'+'))
}

/// A stream option set.
///
/// The stream it replaces is given up here, as php's
/// `zval_ptr_dtor(&ch->handlers.write->stream)` gives it up, so a user
/// wrapper's `stream_close` that throws leaves its throwable pending in
/// the body that set the option, and what an option after it raises in
/// `curl_setopt_array` is chained onto it ([`Pending::step`]):
///
/// ```php
/// class W { public $context; function stream_open($p,$m,$o,&$op){return true;} function stream_write($d){return strlen($d);} function stream_eof(){return true;} function stream_close(){ echo "close\n"; throw new Exception("close"); } }
/// stream_wrapper_register('ww', 'W');
/// $h = curl_init();
/// curl_setopt($h, CURLOPT_FILE, fopen('ww://x', 'w'));
/// try { curl_setopt_array($h, [CURLOPT_FILE => fopen('php://memory', 'w'), CURLOPT_URL => "a\0b"]); } catch (Throwable $e) { echo get_class($e), " <- ", $e->getPrevious()->getMessage(), "\n"; }
/// ```
/// ```expect
/// close
/// ValueError <- close
/// ```
fn set_stream(
    req: &mut Request,
    object: PhpObject,
    which: StreamOpt,
    value: &Value,
    pending: &mut Pending,
) -> Set {
    let value = value.deref();
    let what = if matches!(value.as_ref(), ValueRef::Null) {
        None
    } else {
        Some(stream_of(req, &value)?)
    };
    if let Some((stream, _)) = what
        && which != StreamOpt::Infile
        && !writable(req, stream)?
    {
        let text = format!(
            "{}(): The provided file handle must be writable",
            req.frames().active_function_name()
        );
        return Err(value_error(req, &text));
    }
    let kept = what.map(|_| Owner::take(req, value));
    let curl = match payload(req, object) {
        Ok(curl) => curl,
        Err(thrown) => {
            if let Some(kept) = kept {
                kept.give_up(req);
            }
            return Err(thrown);
        }
    };
    let old = match which {
        StreamOpt::File => {
            curl.write.to = if kept.is_some() {
                WriteTo::File
            } else {
                WriteTo::Stdout
            };
            std::mem::replace(&mut curl.write.stream, kept)
        }
        StreamOpt::WriteHeader => {
            curl.header.to = if kept.is_some() {
                HeaderTo::File
            } else {
                HeaderTo::Ignore
            };
            std::mem::replace(&mut curl.header.stream, kept)
        }
        StreamOpt::Infile => std::mem::replace(&mut curl.read.stream, kept),
        StreamOpt::Stderr => std::mem::replace(&mut curl.stderr, kept),
    };
    let mut code = 0;
    if which == StreamOpt::Stderr {
        let easy = match easy_of(req, object) {
            Ok(easy) => easy,
            Err(thrown) => {
                if let Some(old) = old {
                    old.give_up(req);
                }
                return Err(thrown);
            }
        };
        let thrown;
        (code, thrown) = easy.set_stderr(req, what);
        if let Some(thrown) = thrown {
            pending.park(req, thrown);
        }
        if let Some(old) = old {
            easy.let_stream_go(req, old);
        }
    } else if let Some(old) = old {
        old.give_up(req);
    }
    saved(req, object, code)
}

/// A list option: each element's string, one line each.
fn set_list(
    req: &mut Request,
    object: PhpObject,
    option: i64,
    value: &Value,
    pending: &mut Pending,
) -> Set {
    let Some(array) = value.deref().as_array() else {
        let text = format!(
            "{}(): The {} option must have an array value",
            req.frames().active_function_name(),
            list_name(option)
        );
        return Err(type_error(req, &text));
    };
    let mut lines = Vec::new();
    // `ZEND_HASH_FOREACH_VAL(ph, current)` (`Z/ext/curl/interface.c:2131`),
    // no owner of the table taken: the option's argument holds it while
    // `__toString` runs.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let element = row.value().deref();
        lines.push(with_string(req, &element, <[u8]>::to_vec)?);
        Ok(ControlFlow::Continue(()))
    })?;
    let Ok(list) = Slist::of(&lines) else {
        docref(req, Level::Warning, "Could not build curl_slist");
        return Ok(false);
    };
    let easy = easy_of(req, object)?;
    let called = easy.set_slist(req, option, list);
    set(req, object, called, pending)
}

/// `CURLOPT_POSTFIELDS`: an array as a MIME form, anything else as the
/// body's bytes.
fn set_post_fields(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
    pending: &mut Pending,
) -> Set {
    let value = value.deref();
    if let Some(array) = value.as_array() {
        if array.is_empty() {
            let easy = easy_of(req, object)?;
            let called = easy.set_post_fields(req, b"");
            return set(req, object, called, pending);
        }
        return crate::file::build_mime(req, object, array, pending);
    }
    let body = with_string(req, &value, <[u8]>::to_vec)?;
    let easy = easy_of(req, object)?;
    let called = easy.set_post_fields(req, &body);
    set(req, object, called, pending)
}

/// `CURLOPT_SHARE`: a share kept by the easy one and by the
/// object; anything else is ignored, as php ignores it.
fn set_share(req: &mut Request, object: PhpObject, value: &Value, pending: &mut Pending) -> Set {
    let Some(share) = value.deref().as_object() else {
        return saved(req, object, 0);
    };
    let Some(handle) = crate::share::share_of(req, share) else {
        return saved(req, object, 0);
    };
    let (_, thrown) = easy_of(req, object)?.set_share(req, handle);
    if let Some(thrown) = thrown {
        pending.park(req, thrown);
    }
    let held = Owner::take(req, share);
    let old = match payload(req, object) {
        Ok(curl) => curl.share.replace(held),
        Err(thrown) => {
            held.give_up(req);
            return Err(thrown);
        }
    };
    if let Some(old) = old {
        old.give_up(req);
    }
    saved(req, object, 0)
}

/// `_php_curl_setopt`, what a callback the library served left in
/// `pending`.
#[allow(clippy::too_many_lines)]
fn setopt(
    req: &mut Request,
    object: PhpObject,
    option: i64,
    value: &Value,
    in_array: bool,
    pending: &mut Pending,
) -> Set {
    let Some(kind) = decode(option) else {
        let tail = if in_array {
            "must contain only valid cURL options"
        } else {
            "is not a valid cURL option"
        };
        payload(req, object)?.err = 48;
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, 2, tail));
    };
    let easy: EasyId = easy_of(req, object)?;
    match kind {
        Opt::Channel(chan) => set_channel(req, object, chan, value, option, in_array),
        Opt::Hook(hook) => set_hook(req, object, hook, value, option, in_array, pending),
        Opt::VerifyHost => {
            let n = to_long(req, &value.deref())?;
            if n == 1 {
                docref(
                    req,
                    Level::Notice,
                    "CURLOPT_SSL_VERIFYHOST no longer accepts the value 1, value 2 will be used instead",
                );
                let called = easy.set_long(req, option, 2);
                return set(req, object, called, pending);
            }
            let called = easy.set_long(req, option, n);
            set(req, object, called, pending)
        }
        Opt::Long | Opt::Protocols => {
            let n = to_long(req, &value.deref())?;
            if kind == Opt::Protocols && basedir(req) && n & c::CURLPROTO_FILE != 0 {
                docref(
                    req,
                    Level::Warning,
                    "CURLPROTO_FILE cannot be activated when an open_basedir is set",
                );
                return Ok(false);
            }
            let called = easy.set_long(req, option, n);
            set(req, object, called, pending)
        }
        Opt::SafeUpload => {
            if !truthy(req, &value.deref())? {
                let text = format!(
                    "{}(): Disabling safe uploads is no longer supported",
                    req.frames().active_function_name()
                );
                return Err(value_error(req, &text));
            }
            saved(req, object, 0)
        }
        Opt::Str | Opt::ProtocolsStr => {
            let text = with_string(req, value, <[u8]>::to_vec)?;
            if kind == Opt::ProtocolsStr && basedir(req) {
                let lower = text.to_ascii_lowercase();
                let names = |word: &[u8]| lower.windows(word.len()).any(|w| w == word);
                if names(b"file") || names(b"all") {
                    docref(
                        req,
                        Level::Warning,
                        "The FILE protocol cannot be activated when an open_basedir is set",
                    );
                    return Ok(false);
                }
            }
            option_str(req, object, option, &text, pending)
        }
        Opt::NullableStr => {
            if matches!(value.deref().as_ref(), ValueRef::Null) {
                let called = easy.set_str(req, option, None);
                return set(req, object, called, pending);
            }
            let text = with_string(req, value, <[u8]>::to_vec)?;
            option_str(req, object, option, &text, pending)
        }
        Opt::Private => {
            let held = Owner::take(req, value.deref());
            let old = match payload(req, object) {
                Ok(curl) => curl.private.replace(held),
                Err(thrown) => {
                    held.give_up(req);
                    return Err(thrown);
                }
            };
            if let Some(old) = old {
                old.give_up(req);
            }
            Ok(true)
        }
        Opt::Url => {
            let url = with_string(req, value, <[u8]>::to_vec)?;
            set_url(req, object, &url, pending)
        }
        Opt::Stream(which) => set_stream(req, object, which, value, pending),
        Opt::List => set_list(req, object, option, value, pending),
        Opt::Ignored => saved(req, object, 0),
        Opt::PostFields => set_post_fields(req, object, value, pending),
        Opt::ReturnTransfer => {
            let on = truthy(req, &value.deref())?;
            payload(req, object)?.write.to = if on { WriteTo::Return } else { WriteTo::Stdout };
            saved(req, object, 0)
        }
        Opt::OffT => {
            let n = to_long(req, &value.deref())?;
            let called = easy.set_off(req, option, n);
            set(req, object, called, pending)
        }
        Opt::PostRedir => {
            let n = to_long(req, &value.deref())?;
            let called = easy.set_long(req, option, n & c::CURL_REDIR_POST_ALL);
            set(req, object, called, pending)
        }
        Opt::Path => {
            let path = with_string(req, value, <[u8]>::to_vec)?;
            if !path.is_empty()
                && req
                    .environment_mut()
                    .check_open_basedir(&path, true)
                    .is_err()
            {
                return Ok(false);
            }
            option_str(req, object, option, &path, pending)
        }
        Opt::HeaderOut => {
            if payload(req, object)?.hook(Hook::Debug).is_some() {
                return Err(value_error(
                    req,
                    "CURLINFO_HEADER_OUT option must not be set when the CURLOPT_DEBUGFUNCTION option is set",
                ));
            }
            let on = truthy(req, &value.deref())?;
            if let Some(thrown) = easy.set_debug(req, on) {
                pending.park(req, thrown);
            }
            let (_, thrown) = easy.set_long(req, c::CURLOPT_VERBOSE, i64::from(on));
            set(req, object, (0, thrown), pending)
        }
        Opt::Share => set_share(req, object, value, pending),
        Opt::Blob => {
            let blob = with_string(req, value, <[u8]>::to_vec)?;
            let called = easy.set_blob(req, option, &blob);
            set(req, object, called, pending)
        }
    }
}

/// `curl_setopt(CurlHandle $handle, int $option, mixed $value): bool`.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_setopt($c, CURLOPT_URL, 'file:///x'), curl_getinfo($c, CURLINFO_EFFECTIVE_URL));
/// try { curl_setopt($c, 1234, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_setopt($c, CURLOPT_HTTPHEADER, 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_setopt($c, CURLOPT_WRITEFUNCTION, 'nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_setopt($c, CURLOPT_SAFE_UPLOAD, false); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(curl_setopt($c, CURLOPT_SSL_VERIFYHOST, 1));
/// ```
/// ```expect
/// bool(true)
/// string(9) "file:///x"
/// curl_setopt(): Argument #2 ($option) is not a valid cURL option
/// curl_setopt(): The CURLOPT_HTTPHEADER option must have an array value
/// curl_setopt(): Argument #3 ($value) must be a valid callback for option CURLOPT_WRITEFUNCTION, function "nope" not found or invalid function name
/// curl_setopt(): Disabling safe uploads is no longer supported
///
/// Notice: curl_setopt(): CURLOPT_SSL_VERIFYHOST no longer accepts the value 1, value 2 will be used instead in %s on line 8
/// bool(true)
/// ```
///
/// A callable option keeps what php's `zend_fcc_dup` keeps: a
/// `'Class::method'` string set with `$this` in scope keeps the object
/// it resolved to for as long as the option keeps the callable.
///
/// ```php
/// class A {
///     function w($ch, $data) { return strlen($data); }
///     function go() {
///         $ch = curl_init();
///         curl_setopt($ch, CURLOPT_WRITEFUNCTION, 'A::w');
///         return $ch;
///     }
///     function __destruct() { echo "dtor\n"; }
/// }
/// $ch = (new A)->go();
/// echo "kept\n";
/// unset($ch);
/// echo "end\n";
/// ```
/// ```expect
/// kept
/// dtor
/// end
/// ```
///
/// libcurl logs from inside `curl_easy_setopt`: a boolean option given
/// a value past 1 is `infof`'s line (`lib/setopt.c`), which reaches the
/// debug callable, as php calls the library with the `CURL *` live.
///
/// ```php
/// $h = curl_init();
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) { var_dump($type, $data); });
/// var_dump(curl_setopt($h, CURLOPT_VERBOSE, 2));
/// ```
/// ```expect
/// int(0)
/// string(60) "boolean setopt(41) got unsupported argument 2, treated as 1
/// "
/// bool(true)
/// ```
///
/// With no debug callable the line is the `CURLOPT_STDERR` file's, a
/// `php://memory` stream's through its cookie.
///
/// ```php
/// $h = curl_init();
/// curl_setopt($h, CURLOPT_STDERR, $m = fopen('php://memory', 'w+'));
/// var_dump(curl_setopt($h, CURLOPT_VERBOSE, 2));
/// curl_setopt($h, CURLOPT_FAILONERROR, 5);
/// rewind($m);
/// var_dump(stream_get_contents($m));
/// ```
/// ```expect
/// bool(true)
/// string(124) "* boolean setopt(41) got unsupported argument 2, treated as 1
/// * boolean setopt(45) got unsupported argument 5, treated as 1
/// "
/// ```
///
/// What the callable throws stays pending, as `zend_call_known_fcc`
/// leaves it, and is `curl_setopt`'s throwable; `curl_setopt_array`
/// goes on to its next option past it, calling no callable meanwhile.
///
/// ```php
/// $h = curl_init();
/// $n = 0;
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) use (&$n) { $n++; throw new Exception(trim($data)); });
/// try { curl_setopt($h, CURLOPT_VERBOSE, 2); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { curl_setopt_array($h, [CURLOPT_VERBOSE => 3, CURLOPT_PRIVATE => 'p', CURLOPT_FAILONERROR => 4]); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump($n, curl_getinfo($h, CURLINFO_PRIVATE));
/// ```
/// ```expect
/// boolean setopt(41) got unsupported argument 2, treated as 1
/// boolean setopt(41) got unsupported argument 3, treated as 1
/// int(2)
/// string(1) "p"
/// ```
///
/// The `CURL *` is held across the library's call, so the callable may
/// set options on it again, and a set made from another transfer's
/// callback serves the callable of the `CurlHandle` it names.
///
/// ```php
/// $h = curl_init();
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) {
///     echo $data;
///     curl_setopt($ch, CURLOPT_HTTPHEADER, ['X: y']);
///     curl_setopt($ch, CURLOPT_STDERR, fopen('php://memory', 'w+'));
///     curl_setopt($ch, CURLOPT_DEBUGFUNCTION, null);
/// });
/// var_dump(curl_setopt($h, CURLOPT_VERBOSE, 2), curl_setopt($h, CURLOPT_VERBOSE, 2));
/// $g = curl_init();
/// curl_setopt($g, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) { echo "g: $data"; });
/// $f = tempnam(sys_get_temp_dir(), 'curl');
/// file_put_contents($f, "body");
/// $c = curl_init("file://$f");
/// curl_setopt($c, CURLOPT_WRITEFUNCTION, function ($c, $data) use ($g) {
///     var_dump(curl_setopt($g, CURLOPT_VERBOSE, 3));
///     return strlen($data);
/// });
/// var_dump(curl_exec($c));
/// unlink($f);
/// ```
/// ```expect
/// boolean setopt(41) got unsupported argument 2, treated as 1
/// bool(true)
/// bool(true)
/// g: boolean setopt(41) got unsupported argument 3, treated as 1
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn curl_setopt(
    req: &mut Request,
    handle: Instance<CurlHandle>,
    option: i64,
    value: Value,
) -> Result<bool, Thrown> {
    let mut pending = Pending::default();
    let answered = setopt(req, handle.object(), option, &value, false, &mut pending);
    pending.answer(req, answered)
}

/// `curl_setopt_array(CurlHandle $handle, array $options): bool`: each
/// option in order, stopping at the first that fails.
///
/// ```php
/// $c = curl_init();
/// var_dump(curl_setopt_array($c, [CURLOPT_URL => 'file:///y', CURLOPT_RETURNTRANSFER => true]));
/// try { curl_setopt_array($c, ['x' => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { curl_setopt_array($c, [1234 => 1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// curl_setopt_array(): Argument #2 ($options) contains an invalid cURL option
/// curl_setopt_array(): Argument #2 ($options) must contain only valid cURL options
/// ```
///
/// An `exit` a callable takes stands as php's unwind exit does, and the
/// options after it are set:
///
/// ```php
/// $h = curl_init();
/// curl_setopt($h, CURLOPT_DEBUGFUNCTION, function ($ch, $type, $data) { echo $data; exit("exit\n"); });
/// register_shutdown_function(function () use ($h) { var_dump(curl_getinfo($h, CURLINFO_PRIVATE)); });
/// curl_setopt_array($h, [CURLOPT_VERBOSE => 2, CURLOPT_PRIVATE => 'p']);
/// echo "unreached\n";
/// ```
/// ```expect
/// boolean setopt(41) got unsupported argument 2, treated as 1
/// exit
/// string(1) "p"
/// ```
#[php_function]
fn curl_setopt_array(
    req: &mut Request,
    handle: Instance<CurlHandle>,
    options: PhpArray,
) -> Result<bool, Thrown> {
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(arr), …)`
    // (`Z/ext/curl/interface.c:2357`), no owner of the table taken: the
    // argument holds it while each option's conversion and callback run.
    // What a callback left pending stands over the options after it, as
    // php's `EG(exception)` does.
    let mut pending = Pending::default();
    let walked = walk::foreach(req, options, Hold::None, |req, row| -> walk::Step<bool> {
        let key = row.key().clone();
        let value = row.value().deref();
        let set = pending.step(req, |req, pending| {
            let Key::Int(option) = key else {
                let ce = Throwables::of(req)?;
                return Err(argument_error(
                    req,
                    ce.value_error,
                    2,
                    "contains an invalid cURL option",
                ));
            };
            setopt(req, handle.object(), option, &value, true, pending)
        });
        match set {
            Some(Ok(true)) => Ok(ControlFlow::Continue(())),
            // A fatal standing leaves at once.
            Some(Ok(false)) | None => Ok(ControlFlow::Break(false)),
            Some(Err(thrown)) => Err(thrown),
        }
    });
    let answered = walked.map(|ended| ended.unwrap_or(true));
    pending.answer(req, answered)
}

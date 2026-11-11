//! `fpm_log.c`: each pool's access log, opened by the master and written
//! by a child after each request as `access.format` says, a `%` token
//! and its `{…}` modifier at a time, through a stream held to
//! `log_limit`; a successful `GET` or `HEAD` without a query string or a
//! body to a path `access.suppress_path[]` names left out.
//!
//! Each field the request supplies, `%e`, `%f`, `%m`, `%o`, `%q`, `%r`
//! and `%u`, has a `\r` or `\n` in it written as two bytes
//! ([`php_sapi::log_field`]), so no request writes a line of its own;
//! every other byte, and every other token, is php's (#4355).

use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt as _;

use php_sapi::log_field as field;

use crate::master::Fpm;
use crate::scoreboard::{BoardView, ProcView};
use crate::zlog::Stream;
use crate::{zlog, zlog_sys};

/// `fpm_log_open`: each pool's access log opened for appending, `1`
/// where no pool has one; on `reopen` the new file put where the old one
/// stood and every child told to finish, so none writes to the old one.
pub(crate) fn open(fpm: &mut Fpm, reopen: bool) -> i32 {
    let mut ret = 1;
    for p in 0..fpm.pools.len() {
        let Some(log) = fpm.pools[p].config.access_log.clone() else {
            continue;
        };
        ret = 0;
        let opened = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(crate::conf::path(&log));
        let file = match opened {
            Ok(file) => OwnedFd::from(file),
            Err(e) => {
                zlog_sys!(
                    Error @ fpm_log_open:43,
                    crate::conf::errno(&e),
                    bytes "failed to open access log ({})",
                    log
                );
                return -1;
            }
        };
        zlog!(Debug @ fpm_log_open:46, bytes "open access log ({})", log);
        if reopen {
            if let Some(old) = &mut fpm.pools[p].access_log {
                nix::unistd::dup2(std::os::fd::AsFd::as_fd(&file), old).ok();
                nix::fcntl::fcntl(
                    std::os::fd::AsFd::as_fd(old),
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                )
                .ok();
            }
            fpm.kill_all(nix::sys::signal::Signal::SIGQUIT);
        } else {
            fpm.pools[p].access_log = Some(file);
        }
    }
    ret
}

/// A child's access log: the format, the paths it leaves out, and the
/// stream it writes through (`fpm_log_init_child`).
pub(crate) struct AccessLog {
    format: Vec<u8>,
    suppress: Vec<Vec<u8>>,
    stream: Stream,
}

impl AccessLog {
    /// `fpm_log_init_child`: nothing where the pool has no access log.
    pub(crate) fn new(
        fd: Option<OwnedFd>,
        format: Option<Vec<u8>>,
        suppress: &[Vec<u8>],
    ) -> Option<AccessLog> {
        let fd = fd?;
        let format = format?;
        Some(AccessLog {
            format,
            suppress: suppress.iter().rev().cloned().collect(),
            stream: Stream::access(fd),
        })
    }
}

/// What a line of the access log reads from the request besides its
/// `scoreboard` slot.
pub(crate) struct Request<'r> {
    pub(crate) pool: &'r BoardView,
    pub(crate) proc: &'r ProcView,
    /// `SG(sapi_headers).http_response_code`.
    pub(crate) status: i64,
    /// `SG(sapi_headers).headers`.
    pub(crate) headers: &'r [Vec<u8>],
    /// `SG(request_info).content_length`.
    pub(crate) content_length: i64,
    /// `fcgi_getenv`.
    pub(crate) param: &'r dyn Fn(&[u8]) -> Option<Vec<u8>>,
    /// `fcgi_get_last_client_ip`.
    pub(crate) client_ip: Option<String>,
}

/// `fpm_access_log_suppress`.
fn suppressed(log: &AccessLog, request: &Request<'_>) -> bool {
    let proc = request.proc;
    if !proc.query_string.is_empty()
        || !(proc.request_method == b"GET" || proc.request_method == b"HEAD")
        || !(200..=299).contains(&request.status)
        || request.content_length > 0
    {
        return false;
    }
    log.suppress.contains(&proc.request_uri)
}

/// `fpm_log_write(access.format)` with no request: whether the format
/// holds only tokens php knows, each modifier where one is allowed.
pub(crate) fn check(format: &[u8]) -> bool {
    run(format, None, &mut None)
}

/// `fpm_log_write(NULL)`: one line for the request, where it is not left
/// out.
pub(crate) fn write(log: &mut AccessLog, request: &Request<'_>) {
    if suppressed(log, request) {
        return;
    }
    let format = log.format.clone();
    log.stream.start();
    let mut stream = Some(&mut log.stream);
    run(&format, Some(request), &mut stream);
    log.stream.finish();
}

/// `strftime` of a time in the process's zone into php's 129-byte buffer.
fn time(format: &[u8], epoch: i64) -> Vec<u8> {
    crate::ffi::strftime(format, epoch, 128)
}

/// The walk behind [`check`] and [`write`]: `request` absent is php's
/// `test`.
#[allow(clippy::too_many_lines)] // php's one function, kept as one
fn run(format: &[u8], request: Option<&Request<'_>>, stream: &mut Option<&mut Stream>) -> bool {
    let mut s = 0;
    let mut token = false;
    let mut modifier: Vec<u8> = Vec::new();
    let out = |stream: &mut Option<&mut Stream>, bytes: &[u8]| {
        stream.as_mut().map_or(0, |s| s.str(bytes))
    };
    let over = |stream: &Option<&mut Stream>| stream.as_ref().is_some_and(|s| s.is_over_limit());
    while s < format.len() {
        let c = format[s];
        if !token && c == b'%' {
            token = true;
            modifier.clear();
            s += 1;
            continue;
        }
        if token {
            token = false;
            let m = modifier.clone();
            let m_is = |word: &str| m.eq_ignore_ascii_case(word.as_bytes());
            let warn_modifier = |allowed: &str, line: u32| {
                zlog!(
                    Warning @ (crate::zlog::Site::at("fpm_log_write", line)),
                    bytes "only {} are allowed as a modifier for %{} ('{}')",
                    allowed,
                    char::from(c),
                    m
                );
            };
            match c {
                b'%' => {
                    if let Some(st) = stream.as_mut() {
                        st.char(b'%');
                    }
                }
                b'C' => {
                    if !(m.is_empty() || m_is("total") || m_is("user") || m_is("system")) {
                        warn_modifier("'total', 'user' or 'system'", 190);
                        return false;
                    }
                    modifier.clear();
                    if let Some(r) = request {
                        out(stream, format!("{:.2}", r.proc.cpu()).as_bytes());
                    }
                }
                b'd' => {
                    let d = request.map_or(0, |r| r.proc.duration_us);
                    #[allow(clippy::cast_precision_loss)]
                    let text = if m.is_empty() || m_is("seconds") {
                        format!("{:.3}", d as f64 / 1_000_000.0)
                    } else if m_is("milliseconds")
                        || m_is("milli")
                        || m_is("miliseconds")
                        || m_is("mili")
                    {
                        format!("{:.3}", d as f64 / 1000.0)
                    } else if m_is("microseconds") || m_is("micro") {
                        format!("{d}")
                    } else {
                        warn_modifier(
                            "'seconds', 'milli', 'milliseconds', 'micro' or 'microseconds'",
                            224,
                        );
                        return false;
                    };
                    modifier.clear();
                    if request.is_some() {
                        out(stream, text.as_bytes());
                    }
                }
                b'e' => {
                    if m.is_empty() {
                        zlog!(
                            Warning @ fpm_log_write:232,
                            "the name of the environment variable must be set between embraces for %{}",
                            char::from(c)
                        );
                        return false;
                    }
                    if let Some(r) = request {
                        let value = (r.param)(&m).unwrap_or_else(|| b"-".to_vec());
                        out(stream, &field(&value));
                    }
                    modifier.clear();
                }
                b'f' => {
                    if let Some(r) = request {
                        let script: &[u8] = if r.proc.script_filename.is_empty() {
                            b"-"
                        } else {
                            &r.proc.script_filename
                        };
                        out(stream, &field(script));
                    }
                }
                b'l' => {
                    if let Some(r) = request {
                        out(stream, r.proc.content_length.to_string().as_bytes());
                    }
                }
                b'm' => {
                    if let Some(r) = request {
                        let method: &[u8] = if r.proc.request_method.is_empty() {
                            b"-"
                        } else {
                            &r.proc.request_method
                        };
                        out(stream, &field(method));
                    }
                }
                b'M' => {
                    let memory = request.map_or(0, |r| r.proc.memory);
                    let text = if m.is_empty() || m_is("bytes") {
                        memory.to_string()
                    } else if m_is("kilobytes") || m_is("kilo") {
                        (memory / 1024).to_string()
                    } else if m_is("megabytes") || m_is("mega") {
                        (memory / 1024 / 1024).to_string()
                    } else {
                        warn_modifier("'bytes', 'kilo', 'kilobytes', 'mega' or 'megabytes'", 281);
                        return false;
                    };
                    modifier.clear();
                    if request.is_some() {
                        out(stream, text.as_bytes());
                    }
                }
                b'n' => {
                    if let Some(r) = request {
                        let pool: &[u8] = if r.pool.pool.is_empty() {
                            b"-"
                        } else {
                            &r.pool.pool
                        };
                        out(stream, pool);
                    }
                }
                b'o' => {
                    if m.is_empty() {
                        zlog!(
                            Warning @ fpm_log_write:295,
                            "the name of the header must be set between embraces for %{}",
                            char::from(c)
                        );
                        return false;
                    }
                    if let Some(r) = request {
                        let mut written = 0;
                        for header in r.headers {
                            if header.is_empty() || crate::conf::find(header, &m).is_none() {
                                continue;
                            }
                            if header.len() <= m.len() + 2
                                || header[m.len()] != b':'
                                || header[m.len() + 1] != b' '
                            {
                                continue;
                            }
                            let value = &header[m.len() + 2..];
                            written +=
                                out(stream, &field(if value.is_empty() { b"-" } else { value }));
                            break;
                        }
                        if written == 0
                            && let Some(st) = stream.as_mut()
                        {
                            st.char(b'-');
                        }
                    }
                    modifier.clear();
                }
                b'p' => {
                    if request.is_some() {
                        out(stream, std::process::id().to_string().as_bytes());
                    }
                }
                b'P' => {
                    if request.is_some() {
                        out(
                            stream,
                            nix::unistd::getppid().as_raw().to_string().as_bytes(),
                        );
                    }
                }
                b'q' => {
                    if let Some(r) = request {
                        out(stream, &field(&r.proc.query_string));
                    }
                }
                b'Q' => {
                    if let Some(r) = request {
                        out(
                            stream,
                            if r.proc.query_string.is_empty() {
                                b""
                            } else {
                                b"?"
                            },
                        );
                    }
                }
                b'r' => {
                    if let Some(r) = request {
                        out(stream, &field(&r.proc.request_uri));
                    }
                }
                b'R' => {
                    if let Some(r) = request {
                        out(stream, r.client_ip.as_deref().unwrap_or("-").as_bytes());
                    }
                }
                b's' => {
                    if let Some(r) = request {
                        out(stream, r.status.to_string().as_bytes());
                    }
                }
                b't' | b'T' => {
                    if let Some(r) = request {
                        let at = if c == b't' {
                            r.proc.accepted_epoch
                        } else {
                            crate::scoreboard::epoch()
                        };
                        let text = if m.is_empty() {
                            time(b"%d/%b/%Y:%H:%M:%S %z", at)
                        } else {
                            time(&m, at)
                        };
                        out(stream, &text);
                    }
                    modifier.clear();
                }
                b'u' => {
                    if let Some(r) = request {
                        out(stream, &field(&r.proc.auth_user));
                    }
                }
                b'{' => {
                    token = true;
                    let start = s + 1;
                    let mut end = start;
                    while end < format.len() && format[end] != b'}' {
                        end += 1;
                    }
                    if end < format.len() {
                        modifier = format[start..end.min(start + 128)].to_vec();
                    }
                    s = end;
                    if s + 1 >= format.len() {
                        zlog!(Warning @ fpm_log_write:432, "missing closing embrace in the access.format");
                        return false;
                    }
                }
                _ => {
                    zlog!(
                        Warning @ fpm_log_write:439,
                        "Invalid token in the access.format (%{})",
                        char::from(c)
                    );
                    return false;
                }
            }
            if format.get(s) != Some(&b'}') && !modifier.is_empty() {
                zlog!(
                    Warning @ fpm_log_write:444,
                    "embrace is not allowed for modifier %{}",
                    char::from(c)
                );
                return false;
            }
            s += 1;
            if over(stream) {
                zlog!(
                    Notice @ fpm_log_write:450,
                    "the log buffer is over the configured limit. The access log request has been truncated."
                );
                break;
            }
            continue;
        }
        if over(stream) {
            zlog!(
                Notice @ fpm_log_write:457,
                "the log buffer is over the configured limit. The access log request has been truncated."
            );
            break;
        }
        if let Some(st) = stream.as_mut() {
            st.char(c);
        }
        s += 1;
    }
    true
}

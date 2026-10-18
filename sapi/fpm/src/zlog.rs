//! `zlog.c`, php-fpm's log: one line per message with the time and the
//! level before it, or with only the level in a child, whose lines the
//! master reads off the child's pipe; written to the error log, to
//! `stderr` while the master has not launched, or to syslog; a child's
//! message sent to the request's `FCGI_STDERR` too, by the logger a
//! child registers ([`set_external`]). A [`Stream`] is a message built in
//! pieces, wrapped or truncated at `log_limit`, what a child's output
//! caught by the master and the access log are written through.

use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Mutex, MutexGuard};

use nix::errno::Errno;

/// `zlog_bool` levels, `ZLOG_DEBUG` to `ZLOG_ALERT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Debug = 1,
    Notice = 2,
    Warning = 3,
    Error = 4,
    Alert = 5,
}

impl Level {
    /// `level_names`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Notice => "NOTICE",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
            Level::Alert => "ALERT",
        }
    }

    /// `syslog_priorities`.
    fn severity(self) -> nix::syslog::Severity {
        use nix::syslog::Severity;
        match self {
            Level::Debug => Severity::LOG_DEBUG,
            Level::Notice => Severity::LOG_NOTICE,
            Level::Warning => Severity::LOG_WARNING,
            Level::Error => Severity::LOG_ERR,
            Level::Alert => Severity::LOG_ALERT,
        }
    }
}

/// Where in php-src a `zlog` call stands, `__func__` and `__LINE__`,
/// which `zlog_buf_prefix` names before the message at `log_level =
/// debug`: the C function and the line the call's closing parenthesis is
/// on, as clang expands `__LINE__` in a call written over several lines.
/// A stream no `zlog_msg` named a site for names C's `(null)` and line 0,
/// and `fpm_fcgi_log`'s `vzlog("", 0, …)` an empty name and line 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Site {
    function: Option<&'static str>,
    line: u32,
}

impl Site {
    /// The call in `function` whose `)` is on `line`.
    pub(crate) const fn at(function: &'static str, line: u32) -> Site {
        Site {
            function: Some(function),
            line,
        }
    }

    /// `fpm_fcgi_log`: what `fastcgi.c` logs, through `vzlog("", 0, …)`.
    pub(crate) const FCGI: Site = Site::at("", 0);

    /// A stream's while nothing named one, `stream->function == NULL`.
    const UNNAMED: Site = Site {
        function: None,
        line: 0,
    };

    /// `%s(), line %d: `, a NULL name as C's `printf` writes it.
    fn named(self) -> String {
        format!(
            "{}(), line {}: ",
            self.function.unwrap_or("(null)"),
            self.line
        )
    }
}

/// `MAX_BUF_LENGTH`: the most one `zlog` line holds.
const MAX_BUF_LENGTH: usize = 2048;
/// `ZLOG_DEFAULT_LIMIT`.
pub(crate) const DEFAULT_LIMIT: usize = 1024;
/// `ZLOG_MIN_LIMIT`.
pub(crate) const MIN_LIMIT: usize = 512;

/// Where the log goes, `zlog_fd`.
pub(crate) enum Target {
    /// `-1`: the process's `stderr`.
    Stderr,
    /// The error log's descriptor.
    File(OwnedFd),
    /// `ZLOG_SYSLOG`.
    Syslog,
}

/// The external logger a child sets, `sapi_cgi_log_fastcgi`.
pub(crate) type External = fn(Level, &[u8]);

/// `zlog.c`'s statics.
#[allow(clippy::struct_excessive_bools)] // `zlog.c`'s flags, one static each
struct State {
    target: Target,
    target_is_stderr: bool,
    level: Level,
    limit: usize,
    buffering: bool,
    launched: bool,
    is_child: bool,
    external: Option<External>,
}

static STATE: Mutex<State> = Mutex::new(State {
    target: Target::Stderr,
    target_is_stderr: false,
    level: Level::Notice,
    limit: DEFAULT_LIMIT,
    buffering: true,
    launched: false,
    is_child: false,
    external: None,
});

fn state() -> MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `zlog_set_fd`: the target, and whether it is `/dev/stderr`; the one
/// before handed back.
pub(crate) fn set_target(target: Target, is_stderr: bool) -> Target {
    let mut state = state();
    state.target_is_stderr = is_stderr;
    std::mem::replace(&mut state.target, target)
}

/// The error log's descriptor, where the log goes to one, for a reopen
/// to `dup2` the new file onto.
pub(crate) fn with_file<R>(f: impl FnOnce(Option<&mut OwnedFd>) -> R) -> R {
    let mut state = state();
    match &mut state.target {
        Target::File(fd) => f(Some(fd)),
        _ => f(None),
    }
}

/// `zlog_set_level`: the level before.
pub(crate) fn set_level(level: Level) -> Level {
    std::mem::replace(&mut state().level, level)
}

/// `zlog_set_limit`.
pub(crate) fn set_limit(limit: usize) {
    state().limit = limit;
}

/// `zlog_set_buffering`.
pub(crate) fn set_buffering(buffering: bool) {
    state().buffering = buffering;
}

/// `zlog_set_launched`.
pub(crate) fn set_launched() {
    state().launched = true;
}

/// `fpm_globals.is_child`, which drops the time from a line.
pub(crate) fn set_child() {
    state().is_child = true;
}

/// `zlog_set_external_logger`.
pub(crate) fn set_external(external: Option<External>) {
    state().external = external;
}

/// [`print_time`] at the level messages are held to, as
/// `fpm_php_trace_dump` stamps a slow log entry.
pub(crate) fn time_now() -> Vec<u8> {
    print_time(state().level)
}

/// `zlog_print_time`: `[%d-%b-%Y %H:%M:%S] `, the microseconds after the
/// seconds while the level is `debug`.
fn print_time(level: Level) -> Vec<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = i64::try_from(now.as_secs()).unwrap_or(0);
    let mut out = crate::ffi::strftime(b"[%d-%b-%Y %H:%M:%S", secs, 64);
    if level == Level::Debug {
        out.extend_from_slice(format!(".{:06}", now.subsec_micros()).as_bytes());
    }
    out.extend_from_slice(b"] ");
    out
}

/// `zlog_buf_prefix`: the line's head, `[LEVEL] ` under syslog, the time
/// and `LEVEL: ` otherwise, a child's without the time; at `log_level =
/// debug` the call's site after the level, and the master's pid before
/// it.
fn prefix(state: &State, level: Level, site: Site, syslog: bool) -> Vec<u8> {
    let debug = state.level == Level::Debug;
    if syslog {
        let at = if debug { site.named() } else { String::new() };
        return format!("[{}] {at}", level.name()).into_bytes();
    }
    let mut out = if state.is_child {
        Vec::new()
    } else {
        print_time(state.level)
    };
    let head = match (debug, state.is_child) {
        (true, false) => format!(
            "{}: pid {}, {}",
            level.name(),
            std::process::id(),
            site.named()
        ),
        (true, true) => format!("{}: {}", level.name(), site.named()),
        (false, _) => format!("{}: ", level.name()),
    };
    out.extend_from_slice(head.as_bytes());
    out
}

/// `zlog_truncate_buf`: `buf` cut to `size - space` bytes, its last three
/// `...`.
fn truncate(buf: &mut Vec<u8>, size: usize, space: usize) {
    buf.resize(size - space, 0);
    let at = size - 3 - space;
    buf[at..at + 3].copy_from_slice(b"...");
}

/// A write of `bytes` to the target, as `zend_quiet_write` makes it.
fn write_target(target: &Target, bytes: &[u8]) {
    match target {
        Target::File(fd) => {
            nix::unistd::write(fd.as_fd(), bytes).ok();
        }
        _ => {
            std::io::stderr().write_all(bytes).ok();
        }
    }
}

/// `vzlog`: one message at `level` from the call at `site`, with
/// `errno`'s text after it for a `ZLOG_SYSERROR`.
pub(crate) fn log(level: Level, site: Site, message: &[u8], errno: Option<Errno>) {
    let external = state().external;
    if let Some(external) = external {
        let mut buf = message.to_vec();
        if buf.len() >= MAX_BUF_LENGTH {
            truncate(&mut buf, MAX_BUF_LENGTH, 0);
        }
        external(level, &buf);
    }
    log_line(&state(), level, site, message, errno);
}

fn log_line(state: &State, level: Level, site: Site, message: &[u8], errno: Option<Errno>) {
    if level < state.level {
        return;
    }
    let syslog = matches!(state.target, Target::Syslog);
    let mut buf = prefix(state, level, site, syslog);
    let mut truncated = buf.len() > MAX_BUF_LENGTH - 1;
    if !truncated {
        buf.extend_from_slice(message);
        truncated = buf.len() >= MAX_BUF_LENGTH;
    }
    if !truncated && let Some(errno) = errno {
        let text = php_sapi::strerror(&std::io::Error::from_raw_os_error(errno as i32));
        buf.extend_from_slice(format!(": {text} ({})", errno as i32).as_bytes());
        truncated = buf.len() >= state.limit;
    }
    if truncated {
        truncate(&mut buf, state.limit.min(MAX_BUF_LENGTH), 1);
    }
    if syslog {
        nix::syslog::syslog(state.level.severity(), &*String::from_utf8_lossy(&buf)).ok();
        buf.push(b'\n');
    } else {
        buf.push(b'\n');
        write_target(&state.target, &buf);
    }
    if !state.target_is_stderr
        && !matches!(state.target, Target::Stderr)
        && !state.launched
        && level >= Level::Notice
    {
        std::io::stderr().write_all(&buf).ok();
    }
}

/// `zlog` with the message formatted, from the call `@ function:line`
/// names in php-src, or `@ (site)` where the site is chosen as the program
/// runs.
#[macro_export]
macro_rules! zlog {
    ($level:ident @ $function:ident : $line:literal, $($arg:tt)*) => {
        $crate::zlog::log(
            $crate::zlog::Level::$level,
            $crate::zlog::Site::at(stringify!($function), $line),
            format!($($arg)*).as_bytes(),
            None,
        )
    };
    ($level:ident @ ($site:expr), $($arg:tt)*) => {
        $crate::zlog::log($crate::zlog::Level::$level, $site, format!($($arg)*).as_bytes(), None)
    };
}

/// `zlog(ZLOG_SYSERROR, …)`: the message and the last error's text.
#[macro_export]
macro_rules! zlog_sys {
    ($level:ident @ $function:ident : $line:literal, $errno:expr, $($arg:tt)*) => {
        $crate::zlog::log(
            $crate::zlog::Level::$level,
            $crate::zlog::Site::at(stringify!($function), $line),
            format!($($arg)*).as_bytes(),
            Some($errno),
        )
    };
    ($level:ident @ ($site:expr), $errno:expr, $($arg:tt)*) => {
        $crate::zlog::log($crate::zlog::Level::$level, $site, format!($($arg)*).as_bytes(), Some($errno))
    };
}

/// `zlog_msg_ex`: `prefix` and `message` as one message of a stream, from
/// the call at `site`.
pub(crate) fn msg(level: Level, site: Site, prefix: &[u8], message: &[u8]) {
    let mut stream = Stream::new(level, prefix.len() + message.len(), Dest::Log, false);
    stream.site = site;
    stream.prefix();
    stream.str(prefix);
    stream.str(message);
    stream.finish();
}

/// Where a stream writes, the descriptor `zlog_stream_init_internal` is
/// handed.
enum Dest {
    /// `0`: the log's own target, `zlog_fd`.
    Log,
    /// `STDERR_FILENO`: the process's `stderr`, which the master tied to
    /// its error log.
    Stderr,
    /// A descriptor of its own, the access log's.
    Fd(OwnedFd),
}

/// `struct zlog_stream`: one message built in pieces.
#[allow(clippy::struct_excessive_bools)] // `struct zlog_stream`'s bit fields
pub(crate) struct Stream {
    level: Level,
    /// `function` and `line`: the call the stream's lines name.
    site: Site,
    use_syslog: bool,
    use_fd: bool,
    dest: Dest,
    use_buffer: bool,
    use_stderr: bool,
    prefix_buffer: bool,
    finished: bool,
    full: bool,
    wrap: bool,
    msg_quote: bool,
    decorate: bool,
    pub(crate) is_stdout: bool,
    over_limit: bool,
    use_external: bool,
    pub(crate) child_pid: i32,
    buf: Vec<u8>,
    buf_size: usize,
    len: usize,
    buf_init_size: usize,
    prefix_len: usize,
    msg_prefix: Option<Vec<u8>>,
    msg_suffix: Option<Vec<u8>>,
    msg_final_suffix: Option<Vec<u8>>,
}

impl Stream {
    /// `zlog_stream_init_internal(stream, level, capacity, fd)`.
    fn new(level: Level, capacity: usize, dest: Dest, access: bool) -> Stream {
        let state = state();
        let (use_syslog, use_fd, fd_is_std) = match &dest {
            Dest::Log => match state.target {
                Target::Syslog => (true, false, false),
                Target::File(_) => (false, true, false),
                Target::Stderr => (false, false, false),
            },
            Dest::Stderr => (false, true, true),
            Dest::Fd(_) => (false, true, false),
        };
        // `fd < 0`: the log's target is `stderr` or syslog.
        let to_stderr_only = matches!(dest, Dest::Log) && !matches!(state.target, Target::File(_));
        let mut stream = Stream {
            level,
            site: Site::UNNAMED,
            use_syslog,
            use_fd,
            dest,
            use_buffer: true,
            use_stderr: false,
            prefix_buffer: false,
            finished: false,
            full: false,
            wrap: false,
            msg_quote: false,
            decorate: false,
            is_stdout: false,
            over_limit: false,
            use_external: false,
            child_pid: 0,
            buf: Vec::new(),
            buf_size: 0,
            len: 0,
            buf_init_size: capacity,
            prefix_len: 0,
            msg_prefix: None,
            msg_suffix: None,
            msg_final_suffix: None,
        };
        if access {
            stream.use_stderr = !use_fd;
        } else {
            stream.use_external = state.external.is_some();
            stream.use_buffer = state.buffering || state.external.is_some() || use_syslog;
            stream.use_stderr = to_stderr_only
                || (!fd_is_std && !use_syslog && !state.launched && level >= Level::Notice);
            stream.prefix_buffer =
                level >= state.level && (stream.use_fd || stream.use_stderr || stream.use_syslog);
        }
        stream
    }

    /// `zlog_stream_init_ex(stream, level, STDERR_FILENO)`, wrapping on:
    /// what the master writes a child's output through.
    pub(crate) fn stderr(level: Level) -> Stream {
        let mut stream = Stream::new(level, 1024, Dest::Stderr, false);
        stream.wrap = true;
        stream
    }

    /// `zlog_stream_init_ex(stream, ZLOG_ACCESS_LOG, fd)` with wrapping
    /// off: the access log's stream.
    pub(crate) fn access(fd: OwnedFd) -> Stream {
        Stream::new(Level::Notice, 1024, Dest::Fd(fd), true)
    }

    /// `zlog_stream_set_decorating`.
    pub(crate) fn set_decorating(&mut self, decorate: bool) {
        self.decorate = decorate;
        if !decorate {
            self.msg_quote = false;
            self.prefix_buffer = false;
        }
    }

    /// `zlog_stream_set_msg_quoting`.
    pub(crate) fn set_msg_quoting(&mut self, quote: bool) {
        self.msg_quote = quote && self.decorate;
    }

    /// `zlog_stream_set_msg_prefix`, cut at `MAX_WRAPPING_PREFIX_LENGTH`.
    pub(crate) fn set_msg_prefix(&mut self, prefix: &[u8]) {
        if self.decorate {
            self.msg_prefix = Some(prefix[..prefix.len().min(510)].to_vec());
        }
    }

    /// `zlog_stream_set_msg_suffix`.
    pub(crate) fn set_msg_suffix(&mut self, suffix: Option<&[u8]>, final_suffix: Option<&[u8]>) {
        if !self.wrap || !self.decorate {
            return;
        }
        if let Some(suffix) = suffix {
            self.msg_suffix = Some(suffix.to_vec());
        }
        if let Some(final_suffix) = final_suffix {
            self.msg_final_suffix = Some(final_suffix.to_vec());
        }
    }

    fn limit() -> usize {
        state().limit
    }

    fn suffix_len(&self) -> usize {
        self.msg_suffix.as_ref().map_or(0, Vec::len)
    }

    /// `zlog_stream_buf_alloc_ex`: room for `needed` bytes, the size
    /// doubled and never past `log_limit`.
    fn alloc_ex(&mut self, needed: usize) {
        let size = if self.buf_size == 0 {
            self.buf_init_size
        } else {
            self.buf_size
        };
        let grown = if self.buf.is_empty() && self.buf_size == 0 {
            size
        } else {
            size << 1
        };
        self.buf_size = Stream::limit().min(grown.max(needed));
        self.buf.resize(self.buf_size, 0);
    }

    /// `zlog_stream_buf_alloc`.
    fn alloc(&mut self) {
        if self.buf_size != 0 && self.buf_size >= Stream::limit().min(self.buf_init_size) {
            return;
        }
        self.alloc_ex(0);
    }

    /// The raw write under the stream: to its descriptor, and to `stderr`
    /// where it writes there too.
    fn direct_write(&self, bytes: &[u8]) {
        if self.use_fd {
            match &self.dest {
                Dest::Fd(fd) => {
                    nix::unistd::write(fd.as_fd(), bytes).ok();
                }
                Dest::Log => write_target(&state().target, bytes),
                Dest::Stderr => {
                    std::io::stderr().write_all(bytes).ok();
                }
            }
        }
        if self.use_stderr {
            std::io::stderr().write_all(bytes).ok();
        }
    }

    /// `zlog_stream_prefix_ex`: the line's head, then the message prefix
    /// and the opening quote.
    pub(crate) fn prefix(&mut self) -> usize {
        if !self.prefix_buffer {
            return 0;
        }
        let state = state();
        let head = prefix(&state, self.level, self.site, self.use_syslog);
        let limit = state.limit;
        drop(state);
        if self.use_buffer {
            self.alloc();
            let mut len = head.len();
            if len + 1 > self.buf_size {
                len = (len + 1).min(limit);
                self.alloc_ex(len);
            }
            let take = head.len().min(self.buf_size);
            self.buf[..take].copy_from_slice(&head[..take]);
            self.len = len.min(self.buf_size);
            self.prefix_len = self.len;
            if let Some(msg_prefix) = self.msg_prefix.clone() {
                self.copy_str(&msg_prefix);
            }
            if self.msg_quote {
                self.copy_char(b'"');
            }
            self.len
        } else {
            let head = &head[..head.len().min(1024)];
            self.direct_write(head);
            let mut written = head.len();
            if let Some(msg_prefix) = &self.msg_prefix {
                self.direct_write(msg_prefix);
                written += msg_prefix.len();
            }
            if self.msg_quote {
                self.direct_write(b"\"");
                written += 1;
            }
            written
        }
    }

    /// `zlog_stream_buf_copy_cstr`.
    fn copy_str(&mut self, bytes: &[u8]) -> usize {
        let mut len = bytes.len();
        if self.buf_size - self.len <= len {
            self.alloc_ex(len + self.len);
        }
        if self.buf_size - self.len <= len {
            self.over_limit = true;
            len = self.buf_size - self.len;
        }
        self.buf[self.len..self.len + len].copy_from_slice(&bytes[..len]);
        self.len += len;
        len
    }

    /// `zlog_stream_buf_copy_char`.
    fn copy_char(&mut self, c: u8) -> usize {
        if self.buf_size == self.len {
            self.alloc_ex(1);
        }
        if self.buf_size == self.len {
            self.over_limit = true;
            return 0;
        }
        self.buf[self.len] = c;
        self.len += 1;
        1
    }

    /// `zlog_stream_buf_flush`: the line to the external logger, less its
    /// head, and with its line end to the target.
    fn buf_flush(&mut self) {
        if self.use_syslog {
            let line = String::from_utf8_lossy(&self.buf[..self.len]).into_owned();
            nix::syslog::syslog(state().level.severity(), &*line).ok();
        }
        if self.use_external {
            let external = state().external;
            if let Some(external) = external {
                external(
                    self.level,
                    &self.buf[self.prefix_len.min(self.len)..self.len],
                );
            }
        }
        self.copy_char(b'\n');
        let line = self.buf[..self.len].to_vec();
        self.direct_write(&line);
        self.len = 0;
    }

    /// `zlog_stream_buf_append`: `bytes` after what the line holds, and
    /// past `log_limit` the line ended there and, wrapping, the rest on
    /// lines of its own.
    fn buf_append(&mut self, bytes: &[u8]) -> usize {
        if self.len == 0 {
            self.len = self.prefix();
        }
        let limit = Stream::limit();
        let reserved = self.len + self.suffix_len() + usize::from(self.msg_quote);
        let over = reserved + bytes.len() >= limit;
        let available = if over {
            self.over_limit = true;
            limit.saturating_sub(reserved + 1)
        } else {
            bytes.len()
        };
        let available = available.min(bytes.len());
        self.copy_str(&bytes[..available]);
        if !over {
            return available;
        }
        if self.wrap {
            if self.msg_quote {
                self.copy_char(b'"');
            }
            if let Some(suffix) = self.msg_suffix.clone() {
                self.copy_str(&suffix);
            }
            self.buf_flush();
            self.prefix();
            return available + self.buf_append(&bytes[available..]);
        }
        let len = self.len;
        let mut line = self.buf[..len].to_vec();
        truncate(&mut line, len, 0);
        self.buf[..line.len()].copy_from_slice(&line);
        self.len = line.len();
        self.full = true;
        available
    }

    /// `zlog_stream_unbuffered_write`.
    fn unbuffered_write(&mut self, bytes: &[u8]) -> usize {
        if self.len == 0 {
            self.len = self.prefix();
        }
        let limit = Stream::limit();
        let reserved = self.len + self.suffix_len() + usize::from(self.msg_quote);
        let required = reserved + bytes.len();
        let mut bytes = bytes;
        let mut append: &[u8] = b"";
        if required >= limit {
            if self.wrap {
                let append: &[u8] = if required == limit { b"" } else { b"\n" };
                let available = limit.saturating_sub(reserved + 1).min(bytes.len());
                self.direct_write(&bytes[..available]);
                if !append.is_empty() {
                    if self.msg_quote {
                        self.direct_write(b"\"");
                    }
                    if let Some(suffix) = &self.msg_suffix {
                        self.direct_write(suffix);
                    }
                    self.direct_write(append);
                }
                self.len = 0;
                return available + self.unbuffered_write(&bytes[available..]);
            }
            self.full = true;
            if required != limit {
                append = b"...";
                let keep = limit.saturating_sub(self.len + append.len());
                bytes = &bytes[..keep.min(bytes.len())];
            }
        }
        self.direct_write(bytes);
        if !append.is_empty() {
            self.direct_write(append);
        }
        self.len += bytes.len();
        bytes.len()
    }

    /// `zlog_stream_start`.
    pub(crate) fn start(&mut self) {
        self.finished = false;
        self.len = 0;
        self.full = false;
        self.over_limit = false;
    }

    /// `zlog_stream_str`.
    pub(crate) fn str(&mut self, bytes: &[u8]) -> usize {
        if bytes.is_empty() || self.full {
            return 0;
        }
        if self.finished {
            self.start();
        }
        if self.use_buffer {
            self.buf_append(bytes)
        } else {
            self.unbuffered_write(bytes)
        }
    }

    /// `zlog_stream_char`.
    pub(crate) fn char(&mut self, c: u8) -> usize {
        if self.full {
            return 0;
        }
        if self.finished {
            self.start();
        }
        if self.use_buffer {
            self.copy_char(c)
        } else {
            self.direct_write(&[c]);
            1
        }
    }

    /// `zlog_stream_finish_buffer_suffix`.
    fn finish_buffer_suffix(&mut self) {
        if self.msg_quote {
            self.copy_char(b'"');
        }
        if let Some(suffix) = self.msg_suffix.clone() {
            self.copy_str(&suffix);
        }
        if let Some(final_suffix) = self.msg_final_suffix.clone() {
            let limit = Stream::limit();
            if self.len + final_suffix.len() >= limit {
                let quoting = self.msg_quote;
                let wrap = self.len + final_suffix.len() + 1 - limit;
                let split = final_suffix.len() - wrap;
                self.copy_str(&final_suffix[..split]);
                self.copy_char(b'\n');
                self.buf_flush();
                self.msg_quote = false;
                self.prefix();
                self.msg_quote = quoting;
                self.copy_str(&final_suffix[split..]);
                self.copy_char(b'\n');
            } else {
                self.copy_str(&final_suffix);
            }
        }
    }

    /// `zlog_stream_finish_direct_suffix`.
    fn finish_direct_suffix(&mut self) {
        if self.msg_quote {
            self.direct_write(b"\"");
            self.len += 1;
        }
        if let Some(suffix) = &self.msg_suffix {
            self.direct_write(suffix);
            self.len += suffix.len();
        }
        if let Some(final_suffix) = self.msg_final_suffix.clone() {
            let limit = Stream::limit();
            if self.len + final_suffix.len() >= limit {
                let quoting = self.msg_quote;
                let wrap = self.len + final_suffix.len() + 1 - limit;
                let split = final_suffix.len() - wrap;
                self.direct_write(&final_suffix[..split]);
                self.direct_write(b"\n");
                self.msg_quote = false;
                self.prefix();
                self.msg_quote = quoting;
                self.direct_write(&final_suffix[split..]);
                self.direct_write(b"\n");
            } else {
                self.direct_write(&final_suffix);
                self.direct_write(b"\n");
            }
        } else {
            self.direct_write(b"\n");
        }
    }

    /// `zlog_stream_finish`: the line ended and written.
    pub(crate) fn finish(&mut self) {
        if self.finished || self.len == 0 {
            return;
        }
        if self.use_buffer {
            if self.decorate {
                self.finish_buffer_suffix();
            }
            self.buf_flush();
        } else if self.decorate {
            self.finish_direct_suffix();
        } else {
            self.direct_write(b"\n");
        }
        self.finished = true;
    }

    /// `zlog_stream_is_over_limit`.
    pub(crate) fn is_over_limit(&self) -> bool {
        self.over_limit
    }
}

#[cfg(test)]
mod tests {
    use super::{Level, Site, State, Target, prefix};

    fn state(level: Level, is_child: bool) -> State {
        State {
            target: Target::Stderr,
            target_is_stderr: false,
            level,
            limit: super::DEFAULT_LIMIT,
            buffering: true,
            launched: false,
            is_child,
            external: None,
        }
    }

    /// The head without the master's `[time] `, which is the clock's.
    fn head(state: &State, level: Level, site: Site, syslog: bool) -> String {
        let text = String::from_utf8_lossy(&prefix(state, level, site, syslog)).into_owned();
        text.split_once("] ")
            .filter(|_| !state.is_child && !syslog)
            .map_or(text.clone(), |(_, rest)| rest.to_owned())
    }

    #[test]
    fn a_debug_line_names_the_call_as_zlog_buf_prefix_does() {
        let at = Site::at("fpm_init", 83);
        let pid = std::process::id();
        let master = state(Level::Debug, false);
        assert_eq!(
            head(&master, Level::Notice, at, false),
            format!("NOTICE: pid {pid}, fpm_init(), line 83: ")
        );
        let child = state(Level::Debug, true);
        assert_eq!(
            head(&child, Level::Warning, at, false),
            "WARNING: fpm_init(), line 83: "
        );
        assert_eq!(
            head(&master, Level::Debug, at, true),
            "[DEBUG] fpm_init(), line 83: "
        );
        assert_eq!(
            head(&master, Level::Error, Site::FCGI, false),
            format!("ERROR: pid {pid}, (), line 0: ")
        );
        assert_eq!(
            head(&master, Level::Warning, Site::UNNAMED, false),
            format!("WARNING: pid {pid}, (null)(), line 0: ")
        );
    }

    #[test]
    fn above_debug_the_site_is_not_named() {
        let at = Site::at("fpm_init", 83);
        assert_eq!(
            head(&state(Level::Notice, false), Level::Notice, at, false),
            "NOTICE: "
        );
        assert_eq!(
            head(&state(Level::Notice, true), Level::Error, at, false),
            "ERROR: "
        );
        assert_eq!(
            head(&state(Level::Notice, false), Level::Alert, at, true),
            "[ALERT] "
        );
    }
}

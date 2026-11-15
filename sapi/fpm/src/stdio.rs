//! `fpm_stdio.c`: the master's stdin and stdout on `/dev/null` and its
//! stderr tied to the error log once it runs; a child's stdout and
//! stderr on pipes the master reads and logs line by line as `child N
//! said into stderr`, where the pool catches its workers' output, and on
//! `/dev/null` otherwise; the error log opened, and reopened on
//! `SIGUSR1`.

use std::ffi::{CStr, CString};
use std::io::Write as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt as _;
use std::sync::OnceLock;

use crate::conf;
use crate::master::{Child, Fpm};
use crate::zlog::{Level, Stream, Target};
use crate::{zlog, zlog_sys};

/// `FPM_STDIO_CMD_FLUSH`, with the NUL `sizeof` counts.
const CMD_FLUSH: &[u8] = b"\0fscf\0";

/// `fpm_stdio_init_main`: stdin and stdout on `/dev/null`.
pub(crate) fn init_main() -> bool {
    let null = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        Ok(null) => OwnedFd::from(null),
        Err(e) => {
            zlog_sys!(
                Error @ fpm_stdio_init_main:32,
                conf::errno(&e),
                "failed to init stdio: open(\"/dev/null\")"
            );
            return false;
        }
    };
    if let Err(e) = nix::unistd::dup2_stdin(&null).and_then(|()| nix::unistd::dup2_stdout(&null)) {
        zlog_sys!(Error @ fpm_stdio_init_main:37, e, "failed to init stdio: dup2()");
        return false;
    }
    true
}

/// `fpm_use_error_log`: whether the log goes to the error log, which is
/// all but a foreground master on a terminal.
pub(crate) fn use_error_log(fpm: &Fpm) -> bool {
    fpm.global.daemonize
        || (!nix::unistd::isatty(std::io::stderr()).unwrap_or(false) && !fpm.force_stderr)
}

/// `fpm_stdio_init_final`: stderr tied to the error log, and the master
/// launched.
pub(crate) fn init_final(fpm: &Fpm) {
    redirect_stderr_to_error_log(fpm);
    crate::zlog::set_launched();
}

/// `fpm_stdio_save_original_stderr`.
pub(crate) fn save_original_stderr(fpm: &mut Fpm) -> bool {
    zlog!(Debug @ fpm_stdio_save_original_stderr:81, "saving original STDERR fd: dup()");
    match nix::unistd::dup(std::io::stderr()) {
        Ok(fd) => {
            fpm.stderr_original = Some(fd);
            true
        }
        Err(e) => {
            zlog_sys!(
                Error @ fpm_stdio_save_original_stderr:84,
                e,
                "failed to save original STDERR fd, access.log records may appear in error_log: dup()"
            );
            false
        }
    }
}

/// `fpm_stdio_restore_original_stderr`.
pub(crate) fn restore_original_stderr(fpm: &mut Fpm, close: bool) {
    let Some(original) = &fpm.stderr_original else {
        return;
    };
    zlog!(Debug @ fpm_stdio_restore_original_stderr:95, "restoring original STDERR fd: dup2()");
    if let Err(e) = nix::unistd::dup2_stderr(original) {
        zlog_sys!(
            Error @ fpm_stdio_restore_original_stderr:97,
            e,
            "failed to restore original STDERR fd, access.log records may appear in error_log: dup2()"
        );
    } else if close {
        fpm.stderr_original = None;
    }
}

/// `fpm_stdio_redirect_stderr_to_error_log`, and its syslog twin that
/// puts stderr on `/dev/null`.
pub(crate) fn redirect_stderr_to_error_log(fpm: &Fpm) {
    if !use_error_log(fpm) {
        return;
    }
    crate::zlog::with_file(|file| {
        if let Some(file) = file
            && let Err(e) = nix::unistd::dup2_stderr(file.as_fd())
        {
            zlog_sys!(
                Error @ fpm_stdio_redirect_stderr_to_error_log:117,
                e,
                "failed to tie stderr fd with error_log fd: dup2()"
            );
        }
    });
    if fpm
        .global
        .error_log
        .as_deref()
        .is_some_and(|l| l.eq_ignore_ascii_case(b"syslog"))
    {
        nix::unistd::dup2_stderr(std::io::stdout()).ok();
    }
}

/// `fpm_stdio_open_error_log`: syslog, or the file opened for appending,
/// the log's target where it is used; on `reopen` the new file put where
/// the old one stood.
pub(crate) fn open_error_log(fpm: &mut Fpm, reopen: bool) -> bool {
    let error_log = fpm.global.error_log.clone().unwrap_or_default();
    if error_log.eq_ignore_ascii_case(b"syslog") {
        openlog(
            syslog_ident(fpm.global.syslog_ident.as_deref()),
            facility(fpm.global.syslog_facility),
        );
        if use_error_log(fpm) {
            crate::zlog::set_target(Target::Syslog, false);
        }
        return true;
    }
    let opened = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC)
        .open(conf::path(&error_log));
    let file = match opened {
        Ok(file) => OwnedFd::from(file),
        Err(e) => {
            zlog_sys!(
                Error @ fpm_stdio_open_error_log:386,
                conf::errno(&e),
                bytes "failed to open error_log ({})",
                error_log
            );
            return false;
        }
    };
    if reopen {
        crate::zlog::with_file(|old| {
            if let Some(old) = old {
                nix::unistd::dup2(file.as_fd(), old).ok();
                nix::fcntl::fcntl(
                    old.as_fd(),
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                )
                .ok();
            }
        });
        return true;
    }
    if use_error_log(fpm) {
        let is_stderr = error_log == b"/dev/stderr" || error_log == b"/proc/self/fd/2";
        crate::zlog::set_target(Target::File(file), is_stderr);
    }
    true
}

/// `openlog(3)` with `LOG_PID | LOG_CONS`, as `fpm_stdio_open_error_log`
/// calls it. glibc keeps the ident's pointer and reads it on every
/// `syslog(3)`, so on Linux `nix` takes the `&'static CStr` itself and hands
/// libc that pointer. Elsewhere `nix` takes an `OsStr` and hands libc a copy
/// that lives for the call alone, which holds where the C library copies the
/// ident in turn, as Apple's does: an ident overwritten after `openlog`
/// still names the process's `syslog` lines on macOS.
fn openlog(ident: &'static CStr, facility: nix::syslog::Facility) {
    #[cfg(not(target_os = "linux"))]
    let ident = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(ident.to_bytes());
    nix::syslog::openlog(
        Some(ident),
        nix::syslog::LogFlags::LOG_PID | nix::syslog::LogFlags::LOG_CONS,
        facility,
    )
    .ok();
}

/// `fpm_global_config.syslog_ident` as syslog holds it: the master's ident,
/// `php-fpm` unless `syslog.ident` names one, up to its first NUL as C reads
/// it, held for the life of the process. The master reads its
/// configuration once per process image (a reload is `execvp`,
/// `fpm_pctl_exec`), so the first ident asked for is the process's, and a
/// reopen on `SIGUSR1` asks for the same one.
fn syslog_ident(named: Option<&[u8]>) -> &'static CStr {
    static IDENT: OnceLock<CString> = OnceLock::new();
    IDENT.get_or_init(|| ident_bytes(named.unwrap_or(b"php-fpm")))
}

/// `bytes` up to the first NUL, as a C string; every byte after it is one
/// C never reads.
fn ident_bytes(bytes: &[u8]) -> CString {
    CString::new(php_proto::c_str(bytes)).unwrap_or_default()
}

/// A `syslog.facility` number as `nix` names it; one it names not, the
/// daemons'.
fn facility(n: i32) -> nix::syslog::Facility {
    use nix::syslog::Facility;
    [
        Facility::LOG_KERN,
        Facility::LOG_USER,
        Facility::LOG_MAIL,
        Facility::LOG_DAEMON,
        Facility::LOG_AUTH,
        Facility::LOG_SYSLOG,
        Facility::LOG_LPR,
        Facility::LOG_NEWS,
        Facility::LOG_UUCP,
        Facility::LOG_LOCAL0,
        Facility::LOG_LOCAL1,
        Facility::LOG_LOCAL2,
        Facility::LOG_LOCAL3,
        Facility::LOG_LOCAL4,
        Facility::LOG_LOCAL5,
        Facility::LOG_LOCAL6,
        Facility::LOG_LOCAL7,
    ]
    .into_iter()
    .find(|f| *f as i32 == n)
    .unwrap_or(Facility::LOG_DAEMON)
}

/// `SIGUSR1`: the error log and the access logs reopened, as a rotation
/// asks.
pub(crate) fn reopen_logs(fpm: &mut Fpm) {
    restore_original_stderr(fpm, false);
    if open_error_log(fpm, true) {
        zlog!(Notice @ fpm_got_signal:112, "error log file re-opened");
    } else {
        zlog!(Error @ fpm_got_signal:114, "unable to re-opened error log file");
    }
    match crate::access::open(fpm, true) {
        0 => zlog!(Notice @ fpm_got_signal:119, "access log file re-opened"),
        -1 => zlog!(Error @ fpm_got_signal:121, "unable to re-opened access log file"),
        _ => {}
    }
    redirect_stderr_to_error_log(fpm);
}

/// `fpm_stdio_child_use_pipes` in a new child: its stdout and stderr on
/// the pipes' write ends, or its stderr on the master's stdout, which is
/// `/dev/null`.
pub(crate) fn child_use_pipes(pipes: Option<[(OwnedFd, OwnedFd); 2]>) {
    match pipes {
        Some([(_, out), (_, err)]) => {
            nix::unistd::dup2_stdout(&out).ok();
            nix::unistd::dup2_stderr(&err).ok();
        }
        None => {
            nix::unistd::dup2_stderr(std::io::stdout()).ok();
        }
    }
}

/// `fpm_stdio_init_child`: the master's error log closed, the log to the
/// child's stderr.
pub(crate) fn init_child() {
    crate::zlog::set_target(Target::Stderr, false);
}

/// `fpm_stdio_flush_child`: the command that ends the line the master is
/// reading from this child.
pub(crate) fn flush_child() {
    std::io::stderr().write_all(CMD_FLUSH).ok();
}

/// One of the two pipes a child's stdout or stderr comes up, where the pool catches
/// its workers' stdout and stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pipe {
    Stdout,
    Stderr,
}

impl Pipe {
    /// php's word for it in `child N said into stdout`.
    fn name(self) -> &'static str {
        match self {
            Pipe::Stdout => "stdout",
            Pipe::Stderr => "stderr",
        }
    }

    /// The read end of this pipe of `child`, where it stands.
    pub(crate) fn of(self, child: &Child) -> Option<&OwnedFd> {
        match self {
            Pipe::Stdout => child.stdout.as_ref(),
            Pipe::Stderr => child.stderr.as_ref(),
        }
    }

    fn of_mut(self, child: &mut Child) -> &mut Option<OwnedFd> {
        match self {
            Pipe::Stdout => &mut child.stdout,
            Pipe::Stderr => &mut child.stderr,
        }
    }
}

/// `fpm_stdio_child_said` on `pipe` of child `pid`, ready in the loop.
pub(crate) fn child_said(fpm: &mut Fpm, pid: i32, pipe: Pipe) {
    let Some((p, c)) = fpm.child(pid) else {
        return;
    };
    let mut child = fpm.pools[p].children.remove(c);
    pipe_said(fpm, p, &mut child, pipe);
    fpm.pools[p].children.insert(c, child);
}

/// `fpm_stdio_child_said` on `pipe` of `child`, who ran in the pool at
/// `pool` and stands outside the pools while it is read: what it wrote
/// read to the end of what stands and logged a line at a time, and the
/// pipe, where it ended, taken out of the loop while the fd is
/// still open and closed after, as php's `fpm_event_del` comes before its
/// `close`.
pub(crate) fn pipe_said(fpm: &mut Fpm, pool: usize, child: &mut Child, pipe: Pipe) {
    let Some(fd) = pipe.of_mut(child).take() else {
        return;
    };
    if said(fpm, pool, child.pid, &mut child.log_stream, &fd, pipe) {
        fpm.unwatch(fd.as_fd());
    } else {
        *pipe.of_mut(child) = Some(fd);
    }
}

/// What `fpm_child_close` does once the events fired: each pipe still
/// open, where something the child started holds its writing end, taken
/// out of the loop and closed.
pub(crate) fn close_pipes(fpm: &mut Fpm, child: &mut Child) {
    for pipe in [Pipe::Stdout, Pipe::Stderr] {
        if let Some(fd) = pipe.of_mut(child).take() {
            fpm.unwatch(fd.as_fd());
        }
    }
}

/// The reading behind [`pipe_said`]: whether the pipe closed.
fn said(
    fpm: &Fpm,
    pool: usize,
    pid: i32,
    stream: &mut Option<Stream>,
    fd: &OwnedFd,
    pipe: Pipe,
) -> bool {
    let stdout = pipe == Pipe::Stdout;
    let config = &fpm.pools[pool].config;
    let what = pipe.name();
    let prefix = crate::zformat!("[pool {}] child {} said into {}: ", config.name, pid, what);
    let create = stream.is_none();
    let log = stream.get_or_insert_with(|| {
        let mut s = Stream::stderr(Level::Warning);
        s.set_decorating(config.decorate_workers_output);
        s.set_msg_prefix(&prefix);
        s.set_msg_quoting(true);
        s.is_stdout = stdout;
        s.child_pid = pid;
        s
    });
    if !create && (log.is_stdout != stdout || log.child_pid != pid) {
        log.finish();
        log.set_msg_prefix(&prefix);
        log.is_stdout = stdout;
        log.child_pid = pid;
    }
    let mut buf = [0u8; 1023];
    let mut cmd_pos = 0usize;
    let mut failed: Option<bool> = None;
    loop {
        let in_buf = match nix::unistd::read(fd, &mut buf) {
            Ok(0) => {
                failed = Some(false);
                break;
            }
            Ok(n) => n,
            Err(nix::errno::Errno::EAGAIN | nix::errno::Errno::EINTR) => break,
            Err(_) => {
                failed = Some(true);
                break;
            }
        };
        let buf = &buf[..in_buf];
        let mut start = 0;
        if cmd_pos > 0 {
            let rest = &CMD_FLUSH[cmd_pos..];
            if rest.len() <= in_buf && buf.starts_with(rest) {
                log.finish();
                start = rest.len();
            } else {
                log.str(&CMD_FLUSH[..cmd_pos]);
            }
            cmd_pos = 0;
        }
        let mut pos = start;
        let mut partial = false;
        while pos < in_buf {
            match buf[pos] {
                b'\n' => {
                    log.str(&buf[start..pos]);
                    log.finish();
                    start = pos + 1;
                }
                0 => {
                    if pos + CMD_FLUSH.len() <= in_buf {
                        if buf[pos..].starts_with(CMD_FLUSH) {
                            log.str(&buf[start..pos]);
                            log.finish();
                            start = pos + CMD_FLUSH.len();
                            pos = start;
                            continue;
                        }
                    } else if CMD_FLUSH.starts_with(&buf[pos..]) {
                        cmd_pos = in_buf - pos;
                        log.str(&buf[start..pos]);
                        partial = true;
                        break;
                    }
                }
                _ => {}
            }
            pos += 1;
        }
        if !partial && start < pos {
            log.str(&buf[start..pos]);
        }
    }
    let Some(errored) = failed else {
        return false;
    };
    if create {
        log.set_msg_suffix(None, Some(b", pipe is closed"));
        log.finish();
    }
    if errored {
        zlog_sys!(
            Error @ fpm_stdio_child_said:271,
            nix::errno::Errno::last(),
            "unable to read what child {pid} said into {what}"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{ident_bytes, syslog_ident};

    #[test]
    fn the_ident_is_read_up_to_its_first_nul_as_c_reads_it() {
        assert_eq!(ident_bytes(b"pool\0www").to_bytes(), b"pool");
        assert_eq!(ident_bytes(b"\xffphp").to_bytes(), b"\xffphp");
        assert_eq!(ident_bytes(b"").to_bytes(), b"");
    }

    #[test]
    fn the_process_holds_one_ident_and_a_reopen_gets_it_again() {
        let first = syslog_ident(None);
        assert_eq!(first.to_bytes(), b"php-fpm");
        assert!(std::ptr::eq(first, syslog_ident(Some(b"php-fpm"))));
    }
}

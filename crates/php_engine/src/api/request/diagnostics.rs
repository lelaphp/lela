//! The diagnostics of a request: `zend_error` through the user's
//! handler, the record a compile or a link holds, and `php_error_cb`'s
//! display, log and `error_get_last()` row; `error_reporting`, with the
//! word `@` writes past the entry; `set_error_handler()`,
//! `set_exception_handler()` and their stacks; what is raised while the
//! engine starts; `display_errors`, `html_errors`, `log_errors` and the
//! XML-RPC fault form; and the assertion callback. Their state is
//! [`DiagnosticsState`]'s, whose fields no file but this one reads.

use php_unit::decl::Bare;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use crate::api::abi::{AfterLog, Callable, IniStage};
use crate::api::abi::{Beneath, chain};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{Charset, Owner, PhpArray, PhpString, Value, ValueRef, adopted_some};
use crate::supplied::Bail;
use crate::supplied::chain::ErrorArgs;

use super::{Ini, Phase, Raised, Request, zone};
use crate::c_number::strtol;

/// The request's diagnostics facet (design §4.4): what the request
/// reports and how it is ending: `error_reporting`, the last error and
/// the error log, how errors are displayed and the user's error handler,
/// the throwable php keeps in `EG(exception)`, and the exit status.
/// [`Request::diagnostics`] reads it and [`Request::diagnostics_mut`]
/// changes it; either is the whole request, since nearly every operation
/// can serve PHP code.
pub struct Diagnostics<R>(pub(crate) R);

impl Request {
    /// The diagnostics facet, to read.
    #[must_use]
    pub fn diagnostics(&self) -> Diagnostics<&Request> {
        Diagnostics(self)
    }

    /// The diagnostics facet, to change.
    pub fn diagnostics_mut(&mut self) -> Diagnostics<&mut Request> {
        Diagnostics(self)
    }
}

/// A diagnostic's message as `php_error_cb` is handed it.
#[derive(Clone, Copy)]
pub(crate) enum Message<'a> {
    /// Bytes the engine formatted, of which `zend_error` makes one
    /// string of the request's with `zend_vstrpprintf` before any
    /// handler reads it.
    Text(&'a [u8]),
    /// A string of the request's a native raised as it stands,
    /// `zend_error_zstr_at`'s, which is `trigger_error()`'s message.
    String(PhpString),
}

/// `php_format_date(format, len, ts, localtime)` (`Z/ext/date/php_date.c:882`),
/// the one function of ext/date php's `main` and its other extensions
/// call: `format` over the timestamp in the request's default zone
/// where `localtime` says, in UTC where it does not. ext/date hands it
/// to [`ExtensionBuilder::register`]; `php_log_err` stamps a file log
/// with it, and `mail.log` its lines. An engine without ext/date stamps
/// in UTC.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct FormatDate(pub FormatDateFn);

/// [`FormatDate`]'s function: the request, the format, the timestamp
/// and whether it is taken in the request's zone.
type FormatDateFn = fn(&mut Request, &[u8], i64, bool) -> Result<Vec<u8>, Thrown>;

/// `php_syslog(priority, "%s", message)` (`Z/main/php_syslog.c`), which
/// shares libc's one connection with userland `openlog()`: ext/standard
/// hands it to [`ExtensionBuilder::register`], and `php_log_err` sends
/// an entry through it at its priority where `error_log` is `syslog`.
/// An engine without it sends that entry to the host's log.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct Syslog(pub fn(&mut Request, i32, &[u8]));

impl Message<'_> {
    /// What the display and the log write, its bytes as raised.
    fn text(&self) -> &[u8] {
        match self {
            Message::Text(text) => text,
            Message::String(s) => s.as_bytes(),
        }
    }
}

/// `PG(last_error_type)`, `last_error_message`, `last_error_file` and
/// `last_error_lineno`: what `php_error_cb` keeps for `error_get_last()`,
/// the message and the file one owner each of strings of the request's,
/// `zend_string_copy`'s, until the next diagnostic, `error_clear_last()`
/// or the request's end gives them back (`clear_last_error`,
/// `Z/main/main.c:1307`).
#[derive(Clone, Copy)]
struct LastError {
    level: Level,
    message: PhpString,
    file: PhpString,
    line: u32,
}

/// `EG(error_reporting)`, the last error, the user handlers and their
/// stacks, the startup's record, the `display_errors` word, the log's
/// guards and the assertion callback, whose fields no file but this one
/// reads.
#[derive(Default)]
pub(super) struct DiagnosticsState {
    /// `EG(error_reporting)`, the mask a diagnostic is held to
    /// ([`Request::php_error_cb`]): what `OnUpdateErrorReporting`
    /// (`Z/Zend/zend.c:111`) reads out of the `error_reporting` entry at
    /// request start and whenever the entry moves, what
    /// `error_reporting()` and `BEGIN_SILENCE` write past the entry, and
    /// what `error_reporting()` answers. php keeps it apart from the ini
    /// entry, which is why `ini_get('error_reporting')` under a `@` reads
    /// the level the `@` masked.
    error_reporting: i64,
    last_error: Option<LastError>,
    /// The diagnostics raised while the engine registers its extensions'
    /// ini entries (`php_during_module_startup()`), held in the order
    /// raised rather than shown, on the one request the engine lays for
    /// that stage; every request it serves raises them again first.
    startup: Option<Vec<(Level, Vec<u8>)>>,
    /// php's `EG(last_fatal_error_backtrace)`, rendered by
    /// `zend_trace_to_string`: the trace `zend_error_impl` fetched for the
    /// fatal being raised under `fatal_error_backtraces`, cleared for
    /// every diagnostic and by `zend_exception_error`, read by
    /// `php_error_cb` for the display and the log forms and handed out
    /// by `error_get_last()` as its `trace`: the array, one owner held
    /// here, beside its rendering.
    last_fatal_error_backtrace: Option<(Option<PhpArray>, String)>,
    /// `EG(error_handling)` at `EH_THROW` with `EG(exception_class)`: the
    /// class a warning is thrown as meanwhile, `None` at `EH_NORMAL`.
    error_handling: Option<String>,
    /// What a warning was thrown as under `EH_THROW`, held until the
    /// native that asked for it hands it on.
    error_handling_thrown: Option<Thrown>,
    /// php's `EG(user_error_handler)`, the handler `set_error_handler`
    /// installed, `IS_UNDEF` as none; the stack under it is
    /// `EG(user_error_handlers)`, each row the handler it replaced and
    /// that handler's mask, `EG(user_error_handlers_error_reporting)`
    /// kept beside it. Every handler here is one owner of its value.
    error_handler: Option<Callable>,
    /// `EG(user_error_handler_error_reporting)`: the levels the handler
    /// is handed, `set_error_handler`'s `$error_levels`.
    error_handler_levels: i64,
    error_handlers: Vec<(Option<Callable>, i64)>,
    /// php's `EG(user_exception_handler)` and the stack under it,
    /// `EG(user_exception_handlers)`, the same shape without a mask.
    exception_handler: Option<Callable>,
    exception_handlers: Vec<Option<Callable>>,
    /// `PG(in_error_log)`: an entry is being logged, from `php_log_err`'s
    /// start to its end, the host's log and what it runs inside it
    /// included, so what that raises is not logged again
    /// (`Z/main/main.c:928`). A host that bails out of its log leaves it
    /// set until the next request starts, as php's `longjmp` passes the
    /// write that clears it.
    in_error_log: bool,
    /// The host bailed out from inside the log it took a fatal into
    /// ([`AfterLog::BailedOut`]), between `php_error_cb`'s log and the
    /// ending that reads it: nothing is displayed and the status is not
    /// written.
    left_in_log: bool,
    /// Whether a diagnostic is displayed past the `php_output` layer,
    /// `php_output_direct`, as php displays what it raises while the
    /// engine starts ([`crate::api::raising::startup`]).
    display_direct: bool,
    /// `PG(display_errors)` where a native wrote it past the ini table
    /// ([`Diagnostics::set_display_errors`]), `None` while it is what the
    /// entry's value reads as; the entry's next change sets it again, as
    /// `OnUpdateDisplayErrors` writes the word.
    display_errors_word: Option<u8>,
}

impl Request {
    /// `EG(error_reporting)` and `other`'s change places, as the
    /// execution state does at a fiber switch
    /// ([`Request::switch_execution`]).
    pub(super) fn swap_error_reporting(&mut self, other: &mut i64) {
        std::mem::swap(&mut self.diagnostics.error_reporting, other);
    }

    /// `EG(user_error_handler_error_reporting) = levels`, the levels the
    /// handler is handed, and the levels it was handed before.
    pub(super) fn replace_error_handler_levels(&mut self, levels: i64) -> i64 {
        std::mem::replace(&mut self.diagnostics.error_handler_levels, levels)
    }

    /// Whether the host bailed out from inside the log it took a fatal
    /// into, read once by the ending that reads it ([`AfterLog::BailedOut`]).
    pub(super) fn take_left_in_log(&mut self) -> bool {
        std::mem::take(&mut self.diagnostics.left_in_log)
    }

    /// `PG(display_errors)` read from the entry again, as
    /// `OnUpdateDisplayErrors` writes the word on the entry's change.
    pub(super) fn reset_display_errors_word(&mut self) {
        self.diagnostics.display_errors_word = None;
    }

    /// `php_log_err_with_severity` (`Z/main/main.c:919-990`): one entry
    /// appended to the file `error_log` names, as it stands when the
    /// entry is written, opened `O_CREAT | O_APPEND` against the working
    /// directory with the permissions `error_log_mode` names where they
    /// lie within `0777`, `0644` where they do not, and written as `[<stamp>] <entry>\n`;
    /// through the registered [`Syslog`] at `priority` where it names
    /// `syslog`; the host's log where no file is named, where it names
    /// `syslog` and no [`Syslog`] is registered, or where the file
    /// cannot be opened. The stamp is `d-M-Y H:i:s e` in the
    /// request's default zone ([`Request::log_stamp`]). Whether the
    /// host's log took it; nothing, and `false`, while an entry is being
    /// logged already, `PG(in_error_log)`'s guard against re-entry.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . "/witness-log-" . getmypid();
    /// ini_set('log_errors', 1); ini_set('display_errors', 0);
    /// ini_set('error_log', $f); ini_set('error_log_mode', '0600');
    /// echo $undefined;
    /// $old = umask(0); error_log("two\nlines"); umask($old);
    /// printf("%o\n", fileperms($f) & 0777);
    /// readfile($f); unlink($f);
    /// ```
    /// ```expect
    /// 600
    /// [%d-%s-%d %d:%d:%d UTC] PHP Warning:  Undefined variable $undefined in %s on line 5
    /// [%d-%s-%d %d:%d:%d UTC] two
    /// lines
    /// ```
    ///
    /// phpdbg's prompt inside a fatal's log is inside the guard, so the
    /// warnings what `ev` serves there raises are not logged. The fatal is
    /// the memory limit's, which php 8.5.12 raises after `str_repeat`
    /// refused its string fallibly, so `$s` is the empty string and the
    /// program runs on once phpdbg is told to continue:
    ///
    /// ```php
    /// ini_set("log_errors", "1"); ini_set("display_errors", "0"); ini_set("memory_limit", "2M");
    /// $s = str_repeat("x", 4 << 20);
    /// echo "after\n";
    /// ```
    /// ```request phpdbg
    /// r
    /// ev $s
    /// ev $nope
    /// c
    /// q
    /// ```
    /// ```expect exit=255
    /// [Successful compilation of %SCRATCH%/witness.php]
    /// prompt> [PHP Fatal error:  Allowed memory size of 2097152 bytes exhausted (tried to allocate 4210688 bytes) in %SCRATCH%/witness.php on line 3]
    /// >00003: $s = str_repeat("x", 4 << 20);
    ///  00004: echo "after\n";
    /// prompt> prompt>%s
    /// prompt> after
    /// [Script ended normally]
    /// prompt>
    /// ```
    fn log_err(&mut self, entry: &[u8], priority: i32) -> bool {
        if self.diagnostics.in_error_log {
            return false;
        }
        self.diagnostics.in_error_log = true;
        let took = self.log_err_guarded(entry, priority);
        self.diagnostics.in_error_log = false;
        took
    }

    /// [`Request::log_err`] inside its guard.
    fn log_err_guarded(&mut self, entry: &[u8], priority: i32) -> bool {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let syslog = self.registered::<Syslog>().next().copied();
        let named = self.ini().get(b"error_log").unwrap_or_default();
        if let (b"syslog", Some(Syslog(syslog))) = (named, syslog) {
            syslog(self, priority, entry);
            return false;
        }
        if named.is_empty() || named == b"syslog" {
            self.log_to_host(entry);
            return true;
        }
        let path = self
            .environment()
            .cwd()
            .join(std::ffi::OsStr::from_bytes(named));
        let mode = self.ini().long(b"error_log_mode");
        let mode = u32::try_from(mode)
            .ok()
            .filter(|mode| (1..=0o777).contains(mode))
            .unwrap_or(0o644);
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(mode)
            .open(path);
        let Ok(mut file) = opened else {
            self.log_to_host(entry);
            return true;
        };
        let stamp = self.log_stamp(std::time::SystemTime::now());
        let mut line = Vec::with_capacity(entry.len() + 32);
        line.push(b'[');
        line.extend_from_slice(&stamp);
        line.extend_from_slice(b"] ");
        line.extend_from_slice(entry);
        line.push(b'\n');
        file.write_all(&line).ok();
        false
    }

    /// `php_format_date("d-M-Y H:i:s e", now, 1)`: through the
    /// [`FormatDate`] the bundle registered, so in the request's default
    /// zone, and in UTC where none is registered or it threw, whose
    /// throwable is left pending as php's `get_timezone_info` leaves
    /// its `DateError`.
    fn log_stamp(&mut self, now: std::time::SystemTime) -> Vec<u8> {
        let utc = zone::utc_log_stamp(now);
        let Some(FormatDate(format)) = self.registered::<FormatDate>().next().copied() else {
            return utc.into_bytes();
        };
        let secs = now
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        match format(self, b"d-M-Y H:i:s e", secs, true) {
            Ok(stamp) => stamp,
            Err(thrown) => {
                self.leave_pending(&thrown);
                utc.into_bytes()
            }
        }
    }

    /// `php_error_cb`: one diagnostic, rendered twice. A repeat
    /// `ignore_repeated_errors` drops is dropped whole; what
    /// `error_reporting` masks is neither logged nor displayed; what is
    /// left goes to the log while `log_errors` is on, in php's log form,
    /// and to the body while `display_errors` is, in whichever of php's
    /// two display forms `html_errors` names, with
    /// `error_prepend_string` and `error_append_string` standing around
    /// it. The user handler takes it first where one stands
    /// ([`Request::note_error_in`]); the message-id table and `EH_THROW`
    /// are the rest of that branch and are not read yet. `true` says the
    /// built-in handler served it.
    pub(crate) fn note_error(
        &mut self,
        level: Level,
        message: &(impl AsRef<[u8]> + ?Sized),
    ) -> bool {
        let (file, line) = self.executed_site();
        self.note_error_in(level, message, &file, line)
    }

    /// `get_filename_lineno`'s site (`Z/Zend/zend.c:1640-1679`) for a
    /// diagnostic raised where the request stands: the executing frame's
    /// file and line, and, where nothing is executing, no file at all,
    /// which `php_error_cb` is handed as `Unknown` at line zero
    /// (`ZEND_STR_UNKNOWN_CAPITALIZED`). A destructor the shutdown walk
    /// refuses to serve raises there, and so does an extension's
    /// request-end function, which `zend_deactivate_modules` serves with
    /// `EG(current_execute_data)` emptied, and what a `zend_catch` raises
    /// while a fatal is in flight, since `_zend_bailout` empties it
    /// first. php's arm for a file being compiled is not taken: the
    /// engine compiles with no site of its own.
    pub(super) fn executed_site(&self) -> (Vec<u8>, u32) {
        if !self.frames().executing() || self.bailout {
            return (b"Unknown".to_vec(), 0);
        }
        let file = self
            .frames()
            .current_file()
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        (file, self.frames().current_line())
    }

    /// The same diagnostic at a file and line of its own, which is what
    /// `zend_exception_error` gives `zend_error_va` for an uncaught
    /// throwable: the site the throwable recorded, not the site the
    /// request has reached.
    ///
    /// The recipe passes both strings empty, `html_errors=0` and
    /// `ignore_repeated_errors=0`, so each witness here sets its entry
    /// with `ini_set()` first. `error_prepend_string` and
    /// `error_append_string` stand around the plain form, with no
    /// newline of their own: the form's own leading newline is what
    /// parts `<PRE>` from `Warning`.
    ///
    /// ```php
    /// ini_set('error_prepend_string', '<PRE>');
    /// ini_set('error_append_string', '</PRE>');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <PRE>
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// </PRE>NAN
    /// ```
    ///
    /// `html_errors` is the other form, `<br />\n<b>Warning</b>:  …`,
    /// with the file and the line in `<b>` and two spaces after the
    /// colon. The message of an `E_ERROR` or an `E_PARSE` is escaped
    /// under it and a warning's stands as it is.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// <br />
    /// <b>Warning</b>:  unexpected NAN value was coerced to string in <b>%s</b> on line <b>3</b><br />
    /// NAN
    /// ```
    ///
    /// The `E_ERROR` an uncaught throwable becomes carries its trace in
    /// its message, and a string argument in the trace is where the
    /// four bytes `escape_html` moves reach a fatal: the argument is
    /// escaped, and the `<b>` around the file and the line are written
    /// after the message and stand.
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// ini_set('<&">');
    /// ```
    /// ```expect exit=255
    /// <br />
    /// <b>Fatal error</b>:  Uncaught ArgumentCountError: ini_set() expects exactly 2 arguments, 1 given in %s:3
    /// Stack trace:
    /// #0 %s(3): ini_set('&lt;&amp;&quot;&gt;')
    /// #1 {main}
    ///   thrown in <b>%s</b> on line <b>3</b><br />
    /// ```
    ///
    /// `ignore_repeated_errors` drops a repeat of the last diagnostic
    /// stored, and while `ignore_repeated_source` is off a repeat is one
    /// at the same file and line: the second `NAN` on line 3 is dropped,
    /// the one on line 4 stands. With `ignore_repeated_source` on the
    /// site no longer parts them, and the one on line 6 is dropped too.
    ///
    /// ```php
    /// ini_set('ignore_repeated_errors', '1');
    /// echo NAN, NAN;
    /// echo NAN;
    /// ini_set('ignore_repeated_source', '1');
    /// echo NAN;
    /// ```
    /// ```expect
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NANNAN
    /// Warning: unexpected NAN value was coerced to string in %s on line 4
    /// NANNAN
    /// ```
    ///
    /// The rest of php's display branch (`Z/main/main.c:1409-1489`),
    /// condition by condition. `E_CORE` stands past `error_reporting`,
    /// `(EG(error_reporting) & type) || (type & E_CORE)`, and the mask
    /// here reads it; nothing in this engine raises `E_CORE_ERROR` or
    /// `E_CORE_WARNING` yet, since php's own raisers of the two are its
    /// own startup and the ini loader, so the mask is held by a test
    /// on this function and no witness reaches it. `xmlrpc_errors` is
    /// the form ahead of the two `html_errors` names, read below;
    /// `PHP_INI_SYSTEM`, so a host's startup ini sets it and no
    /// `ini_set()` does, and it is held by an engine test rather than a
    /// witness, whose recipe passes the entry nothing. The stderr form
    /// is the CLI's, the CGI's and phpdbg's, `sapi_module.name` read at
    /// `Z/main/main.c:1484`; under another host `display_errors=stderr`
    /// falls to the plain form on the body, which [`Request::display_errors`]
    /// answers off the host's name. What is raised while modules start
    /// and while the request starts, before the program is compiled, a
    /// body's reader's warning among it, is displayed only under
    /// `display_startup_errors`, as php's display branch asks
    /// `module_initialized && !during_request_startup` before it.
    ///
    /// A fatal's own trace is php 8.5's `fatal_error_backtraces`
    /// (`Z/Zend/zend.c:263`, `On` by default), the half of
    /// `zend_error_impl` before `zend_error_cb` (`Z/Zend/zend.c:1500-1526`):
    /// `EG(last_fatal_error_backtrace)` is cleared for every diagnostic,
    /// and while the entry is on a level in `E_FATAL_ERRORS` fetches one
    /// where the request stands, `zend_fetch_debug_backtrace` under
    /// `zend.exception_ignore_args`, unless a throwable is pending, which
    /// php reports ahead of the fatal instead. `php_error_cb` renders it
    /// under `Stack trace:` after the site in the log form, the xml
    /// fault, the stderr form and the plain form alike. The recipe
    /// carries `run-tests.php`'s `fatal_error_backtraces=Off`, so a
    /// witness turns the entry on first; the frames are the ones a
    /// throwable's trace walks, so a fatal at a back-edge of the main op
    /// array stands over `{main}` alone.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// A fatal raised inside a native stands over the native's frame, as
    /// a throwable's does, since both are [`Request::trace_as_string`],
    /// whose witnesses pin the frame and its arguments; the one fatal a
    /// native of this engine raises while serving, `var_dump()` over a
    /// `__debugInfo` that answers no array, waits on a compiler that
    /// reads a method, and the oracle prints
    /// `#0 %s(2): var_dump(Object(A))` over `#1 {main}` for it.
    ///
    /// The stderr form carries the trace as the plain form does, on the
    /// error stream with it.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('display_errors', 'stderr'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// ```
    /// ```stderr
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
    /// Stack trace:
    /// #0 {main}
    /// ```
    ///
    /// The trace is what parts php's two html formats
    /// (`Z/main/main.c:1471-1476`): an `E_ERROR` or an `E_PARSE` takes the
    /// first, its message escaped and no trace however the entry stands,
    /// and every other level the second, unescaped, with the trace
    /// between `<br />` and the newline that closes the line. A
    /// `TypeError` left uncaught is the `E_ERROR` an uncaught throwable
    /// becomes, and `zend_exception_error` fetches no trace for it
    /// ([`Request::end_in_uncaught`]), so under html an `E_ERROR` with a
    /// trace is the time limit's or the memory limit's, and it prints
    /// none.
    ///
    /// ```php
    /// ini_set('fatal_error_backtraces', '1'); ini_set('html_errors', '1'); set_time_limit(1); for (;;) {}
    /// ```
    /// ```expect exit=255 wall=1s
    /// <br />
    /// <b>Fatal error</b>:  Maximum execution time of 1 second exceeded in <b>%s</b> on line <b>2</b><br />
    /// ```
    ///
    /// The second html format is `E_USER_ERROR`'s, `E_COMPILE_ERROR`'s,
    /// `E_CORE_ERROR`'s and `E_RECOVERABLE_ERROR`'s; no native here
    /// raises one while serving, so
    /// `the_second_html_format_carries_the_trace_after_the_line` below
    /// holds it to the oracle's bytes for `trigger_error()`.
    pub(crate) fn note_error_in(
        &mut self,
        level: Level,
        message: &(impl AsRef<[u8]> + ?Sized),
        file: &[u8],
        line: u32,
    ) -> bool {
        self.note_message_in(level, Message::Text(message.as_ref()), file, line)
    }

    /// [`Request::note_error`] over a string of the request's raised as
    /// it stands, `zend_error_zstr_at`'s message: the user handler is
    /// handed it and the last error keeps it, one more owner each, as
    /// `trigger_error()`'s string is in php.
    pub(crate) fn note_error_string(&mut self, level: Level, message: PhpString) -> bool {
        let (file, line) = self.executed_site();
        self.note_message_in(level, Message::String(message), &file, line)
    }

    /// [`Request::note_error_in`] over either form of message: what is
    /// held, recorded or thrown reads its text, and what the handlers
    /// are handed is one string of the request's, made once where the
    /// engine formatted the text, as `zend_error_va_list` makes it.
    pub(super) fn note_message_in(
        &mut self,
        level: Level,
        raised: Message,
        file: &[u8],
        line: u32,
    ) -> bool {
        if let Some(held) = self.diagnostics.startup.as_mut() {
            let bytes = match raised {
                Message::Text(text) => text.to_vec(),
                Message::String(string) => string.as_bytes().to_vec(),
            };
            held.push((level, bytes));
            return false;
        }
        let text = raised.text().to_vec();
        let message = text.as_slice();
        if self.recorded(level, message, file, line) {
            return false;
        }
        // `php_error_cb` under `EH_THROW` (`Z/main/main.c:1368`), which
        // `zend_error_impl` reaches past the user handler: a warning is
        // thrown as the class `zend_replace_error_handling` named, where
        // nothing is pending, and shown nowhere.
        if let Some(class) = self.diagnostics.error_handling.clone()
            && matches!(
                level,
                Level::Warning | Level::CoreWarning | Level::CompileWarning | Level::UserWarning
            )
        {
            // The error observers are told before `zend_error_cb`, the
            // first link of which is `php_error_cb`'s throw here.
            crate::observer::error(
                self,
                level,
                file,
                line,
                message,
                crate::observer::Told::Raised,
            );
            if self.raised.is_none() && self.diagnostics.error_handling_thrown.is_none() {
                let thrown = crate::api::raising::throw(self, &class, message);
                self.diagnostics.error_handling_thrown = Some(thrown);
            }
            return false;
        }
        // "Always clear the last backtrace."
        self.clear_fatal_error_backtrace();
        // "Report about uncaught exception in case of fatal errors": the
        // pending throwable rendered as a warning and given up, and no
        // trace is fetched for the fatal (`Z/Zend/zend.c:1503-1526`). An
        // object in the slot that is no `Throwable`, which no PHP code
        // leaves there, stays where it is.
        // The unwind exit `zend_handle_delayed_oom` threw stands as php's
        // `EG(exception)` while the limit's fatal is raised, and is no
        // throwable to report (`Z/Zend/zend.c:1513` at 8.5.12).
        let handling = *self.heap.delayed_oom() == php_heap::DelayedOom::Handling;
        if let Some(Raised::Throwable(pending)) = self.raised {
            let throwable = self
                .class_named(Bare::resolved(b"Throwable"))
                .is_some_and(|throwable| self.symbols().instance_of(pending.class(), throwable));
            if throwable && fatal_errors(level) {
                self.exception_error(Level::Warning).ok();
            }
        } else if (self.raised.is_none() || handling)
            && self.fatal_error_backtraces()
            && fatal_errors(level)
        {
            self.diagnostics.last_fatal_error_backtrace = Some(self.trace_as_string());
        }
        // The string the native raised is its caller's; the one made of
        // the text is this call's, given back once the handlers are done.
        let (string, made) = match raised {
            Message::String(string) => (string, false),
            // The memory limit refusing the message has rendered its
            // fatal, and the request is ending.
            Message::Text(text) => match crate::api::raising::strpprintf(self, text) {
                Ok(string) => (string, true),
                Err(_) => return false,
            },
        };
        // `zend_observer_error_notify` (`Z/Zend/zend.c:1528`): every
        // diagnostic past the record and the pending throwable's report,
        // before the user's handler and `zend_error_cb` are asked.
        crate::observer::error(
            self,
            level,
            file,
            line,
            string.as_bytes(),
            crate::observer::Told::Raised,
        );
        let served = !self.user_error_handler(level, string, file, line);
        if served {
            self.zend_error_cb(level, Bail::Out, string, file, line);
        }
        if made {
            self.release(Value::string(string));
        }
        served
    }

    /// `zend_error_cb`: the links the bundle supplied
    /// ([`ExtensionBuilder::error`]) from the top, each serving the one
    /// beneath where it will, and php's own [`Request::php_error_cb`]
    /// beneath them all.
    ///
    /// [`ExtensionBuilder::error`]: crate::api::abi::ExtensionBuilder::error
    pub(super) fn zend_error_cb(
        &mut self,
        level: Level,
        bail: Bail,
        string: PhpString,
        file: &[u8],
        line: u32,
    ) {
        let shared = Arc::clone(&self.shared);
        let args = ErrorArgs {
            level,
            bail,
            string,
            file,
            line,
        };
        Beneath::<chain::Error>::top(shared.supplied.error.links(), args).serve(self);
    }

    /// `php_error_cb` (`Z/main/main.c:1370-1500`): the diagnostic stored,
    /// masked, logged and displayed, with the trace
    /// [`Request::note_error_in`] fetched, if one, under `Stack trace:`.
    ///
    /// The last error keeps its message, one more owner of the string
    /// raised or the one formatted, and its file, the string the file
    /// was opened under, so what an error leaves is php's: the first
    /// `@` lays down `EG(modified_ini_directives)` as it replaces the
    /// warning's message by `trigger_error()`'s interned `'x'`, a
    /// deprecated method's message replaces that, a repeat replaces it by
    /// its twin, and `error_get_last()` takes no string of its own:
    ///
    /// ```php
    /// $s = new SplObjectStorage; $o = new stdClass; echo $w;
    /// $m = memory_get_usage(); @trigger_error('x', E_USER_DEPRECATED); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); @$s->contains($o); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); @$s->contains($o); echo memory_get_usage() - $m, "\n";
    /// error_clear_last();
    /// $m = memory_get_usage(); @$s->contains($o); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $e = error_get_last(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// Warning: Undefined variable $w in %s on line 2
    /// 320
    /// 160
    /// 0
    /// 160
    /// 376
    /// ```
    ///
    /// Then its last arm, for a level it cannot recover from
    /// ([`Request::cannot_recover`]): the status, the `500` header and
    /// the store marked destructed are `php_error_cb`'s, so a link of
    /// `zend_error_cb` that does not serve it, as soap's serves a
    /// throwing client's fatal, leaves by `zend_bailout` alone.
    pub(crate) fn php_error_cb(&mut self, error: ErrorArgs<'_>) {
        let ErrorArgs {
            level,
            string,
            file,
            line,
            ..
        } = error;
        self.stored_and_shown(level, string, file, line);
        self.cannot_recover(error);
    }

    /// `php_error_cb` up to its last arm: the diagnostic stored unless it
    /// repeats, then logged and displayed unless masked. The html form of
    /// an `E_ERROR` prints the escaped message with `%s`, so a NUL in it
    /// ends it there, where the plain form keeps every byte:
    ///
    /// ```php
    /// ini_set('html_errors', '1');
    /// throw new Exception("before\0after");
    /// ```
    /// ```expect exit=255
    /// <br />
    /// <b>Fatal error</b>:  Uncaught Exception: before in <b>%s</b> on line <b>3</b><br />
    /// ```
    fn stored_and_shown(&mut self, level: Level, string: PhpString, file: &[u8], line: u32) {
        if !self.displayed(string, file, line) {
            return;
        }
        let named = String::from_utf8_lossy(file).into_owned();
        // php stores the diagnostic before it masks it, which is why
        // `error_get_last()` answers one `error_reporting` hid: the
        // message with one more owner and the file name the op array's,
        // `zend_string_copy` of each; the string a file was opened under
        // is the one `EG(included_files)` holds.
        self.clear_last_error_strings();
        let held = self.included_files.held(file);
        if let Some(path) = held {
            self.addref(&Value::string(path));
        }
        if let Some(file) =
            held.map_or_else(|| self.declared_string(file).ok().map(Owner::hand_on), Some)
        {
            self.addref(&Value::string(string));
            self.diagnostics.last_error = Some(LastError {
                level,
                message: string,
                file,
                line,
            });
        }
        // `(EG(error_reporting) & type) || (type & E_CORE)`: a core
        // level is rendered whatever `error_reporting` says.
        let core = matches!(level, Level::CoreError | Level::CoreWarning);
        if self.diagnostics().error_reporting() & level.bit() == 0 && !core {
            return;
        }
        let name = level_name(level);
        let trace = self.stack_trace_after_the_site();
        // What is written is the message's string, its bytes as raised,
        // where php prints it `%S`: the plain form, the stderr form and
        // the html form of a level that is no `E_ERROR` or `E_PARSE`. The
        // log line, the xml fault and the html form of those two print
        // `ZSTR_VAL(message)` with `%s`, to its first NUL
        // (`Z/main/main.c:1493`, `:1499`, `:1507`).
        let whole = string.as_bytes();
        let to_nul = crate::c_number::c_str(whole);
        let said_as = |message: &[u8], before: String, after: &str| -> Vec<u8> {
            [before.as_bytes(), message, after.as_bytes()].concat()
        };
        let said = |before: String, after: &str| said_as(whole, before, after);
        let site = format!(" in {named} on line {line}{trace}");
        if self.log_errors() {
            let text = said_as(to_nul, format!("PHP {name}:  "), &site);
            // `spprintf(&log_buffer, …)`, freed once it is logged.
            let Ok(buffer) = crate::api::raising::printf_buffer(self, text.len()) else {
                return;
            };
            // `syslog_type_int`: `LOG_ERR`, `LOG_WARNING`, `LOG_INFO`
            // for a deprecation, `LOG_NOTICE` for the rest.
            let priority = match level {
                _ if fatal_errors(level) => 3,
                Level::Warning
                | Level::CoreWarning
                | Level::CompileWarning
                | Level::UserWarning => 4,
                Level::Deprecated | Level::UserDeprecated => 6,
                _ => 5,
            };
            let logged = self.log_err(&text, priority);
            self.heap.free_block(buffer);
            if logged && fatal_errors(level) {
                // What follows a fatal in the host's log is still inside
                // `sapi_module.log_message`, so inside `php_log_err`'s
                // guard: a diagnostic it raises is not logged.
                self.diagnostics.in_error_log = true;
                let after = self.fatal_logged();
                if after == AfterLog::BailedOut {
                    self.diagnostics.left_in_log = true;
                    return;
                }
                self.diagnostics.in_error_log = false;
            }
        }
        let display = self.display_errors();
        if display == Display::Off {
            return;
        }
        if self.ini_flag(b"xmlrpc_errors") {
            // php's first display form, one `methodResponse` fault with
            // `xmlrpc_error_number` as its code and the diagnostic as
            // its string, the message unescaped and no space after the
            // colon; neither string stands around it.
            let text = said_as(
                to_nul,
                format!(
                    "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>{}</int></value></member><member><name>faultString</name><value><string>{name}:",
                    self.ini().long(b"xmlrpc_error_number")
                ),
                &format!(
                    "{site}</string></value></member></struct></value></fault></methodResponse>"
                ),
            );
            self.printed(&text);
        } else if self.ini_flag(b"html_errors") {
            // php's two html formats: the message of an `E_ERROR` or an
            // `E_PARSE` escaped and no trace after it, and every other
            // level's as it is with the trace between the `<br />` and
            // the newline.
            let prepend = self.ini_text(b"error_prepend_string");
            let append = self.ini_text(b"error_append_string");
            if let Level::Error | Level::Parse = level {
                self.printed_escaped(
                    &format!("{prepend}<br />\n<b>{name}</b>:  "),
                    to_nul,
                    &format!(" in <b>{named}</b> on line <b>{line}</b><br />\n{append}"),
                );
            } else {
                let text = said(
                    format!("{prepend}<br />\n<b>{name}</b>:  "),
                    &format!(" in <b>{named}</b> on line <b>{line}</b><br />{trace}\n{append}"),
                );
                self.printed(&text);
            }
        } else if display == Display::Stderr {
            // php's stderr form stands inside the same `else` as the
            // plain one, so `html_errors` takes a CLI diagnostic off the
            // error stream and onto the body; and it carries neither
            // string around it.
            let text = said(format!("{name}: "), &format!("{site}\n"));
            self.response_mut().write_stderr(&text);
        } else {
            let before = format!("{}\n{name}: ", self.ini_text(b"error_prepend_string"));
            let after = format!("{site}\n{}", self.ini_text(b"error_append_string"));
            self.printed(&said(before, &after));
        }
    }

    /// [`Request::printed`] of `message` escaped ([`escape_html`]) between
    /// `before` and `after`, with `escape_html`'s string standing while
    /// it is printed; the memory limit refusing that string prints
    /// nothing.
    fn printed_escaped(&mut self, before: &str, message: &[u8], after: &str) {
        let escaped = escape_html(message, self.html_charset());
        let laid = escaped.sizes.last().copied().unwrap_or(24 + 128 + 1);
        let Ok(block) = self.heap.block(laid) else {
            return;
        };
        self.printed(&[before.as_bytes(), &escaped.bytes, after.as_bytes()].concat());
        self.heap.free_block(block);
    }

    /// `php_printf` (`Z/main/main.c:1003`) of a diagnostic's display:
    /// `vspprintf`'s buffer laid down for `text`, written, and freed, so
    /// the peak around a displayed diagnostic holds it as php's does: a
    /// `smart_string`'s 256 bytes for a short one, the page for one past
    /// 255 bytes, and nothing for one `@` silenced. Under `html_errors`
    /// an `E_ERROR`'s message is escaped into a string of its own that
    /// stands beside the buffer.
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo "\n", memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// class K { function m() {} }
    /// pk(function () { trigger_error('x', E_USER_DEPRECATED); });
    /// pk(function () { @trigger_error('x', E_USER_DEPRECATED); });
    /// pk(function () { trigger_error(str_repeat('y', 300), E_USER_DEPRECATED); });
    /// pk(function () { new ReflectionMethod('K::m'); });
    /// ```
    /// ```expect
    /// Deprecated: x in %s on line 4
    ///
    /// Deprecated: x in %s on line 4
    ///
    /// 256 0
    ///
    /// 0 0
    ///
    /// Deprecated: yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy in %s on line 6
    ///
    /// Deprecated: yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy in %s on line 6
    ///
    /// 4096 0
    ///
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 7
    ///
    /// Deprecated: Calling ReflectionMethod::__construct() with 1 argument is deprecated, use ReflectionMethod::createFromMethodName() instead in %s on line 7
    ///
    /// 528 0
    /// ```
    fn printed(&mut self, text: &[u8]) {
        if let Ok(buffer) = crate::api::raising::printf_buffer(self, text.len()) {
            self.display(text);
            self.heap.free_block(buffer);
        }
    }

    /// A diagnostic's display on the body: through the `php_output`
    /// layer, or past it where it is raised as the engine starts.
    fn display(&mut self, text: &[u8]) {
        if self.diagnostics.display_direct {
            self.ub_write(text);
        } else {
            self.response_mut().write(text);
        }
    }

    /// A diagnostic raised as the engine starts, at `Unknown` on line
    /// zero and displayed past the `php_output` layer
    /// ([`crate::api::raising::startup`]).
    pub(crate) fn note_startup_error(&mut self, level: Level, message: &[u8]) {
        let Ok(string) = crate::api::raising::strpprintf(self, message) else {
            return;
        };
        self.diagnostics.display_direct = true;
        self.note_message_in(level, Message::String(string), b"Unknown", 0);
        self.diagnostics.display_direct = false;
        self.release(Value::string(string));
    }

    /// What `php_error_cb` writes after `on line N` for the fatal's trace:
    /// `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""` and the trace, so
    /// nothing at all while none was fetched.
    pub(super) fn stack_trace_after_the_site(&self) -> String {
        self.diagnostics
            .last_fatal_error_backtrace
            .as_ref()
            .map_or_else(String::new, |(_, trace)| format!("\nStack trace:\n{trace}"))
    }

    /// `zval_ptr_dtor(&EG(last_fatal_error_backtrace))` and `ZVAL_UNDEF`.
    pub(super) fn clear_fatal_error_backtrace(&mut self) {
        if let Some((Some(trace), _)) = self.diagnostics.last_fatal_error_backtrace.take() {
            self.release(Value::array(trace));
        }
    }

    /// `fatal_error_backtraces`, `EG(fatal_error_backtrace_on)` through
    /// `OnUpdateBool` (`Z/Zend/zend.c:263`), `PHP_INI_ALL`: whether a
    /// fatal fetches its trace. The engine's default is php's, `1`, which
    /// `fatal_error_backtraces_is_on_by_default` in `engine` holds; the
    /// recipe's `fatal_error_backtraces=Off` reaches the table through the
    /// ini scanner as the empty string, which is what php answers under
    /// the same `-d`, and `ini_set()` moves it.
    ///
    /// ```php
    /// var_dump(ini_get('fatal_error_backtraces'), ini_set('fatal_error_backtraces', '1'), ini_get('fatal_error_backtraces'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// string(0) ""
    /// string(1) "1"
    /// ```
    fn fatal_error_backtraces(&self) -> bool {
        self.ini_flag(b"fatal_error_backtraces")
    }

    /// php's `display`, the first branch of `php_error_cb`: a diagnostic
    /// stands unless `ignore_repeated_errors` is on and it repeats the
    /// last one stored. It repeats when the message is the same and
    /// either `ignore_repeated_source` is on or the site is the same
    /// too. A repeat is dropped whole — not stored, not logged, not
    /// displayed — so the one it repeats stays the last error.
    fn displayed(&self, message: PhpString, file: &[u8], line: u32) -> bool {
        if !self.ini_flag(b"ignore_repeated_errors") {
            return true;
        }
        let Some(last) = self.diagnostics.last_error else {
            return true;
        };
        if last.message.as_bytes() != message.as_bytes() {
            return true;
        }
        if self.ini_flag(b"ignore_repeated_source") {
            return false;
        }
        last.line != line || last.file.as_bytes() != file
    }

    /// `log_errors`, whether a diagnostic reaches the host's log; one
    /// raised as the engine starts, before php's `module_initialized`,
    /// reaches it too while `display_startup_errors` or `display_errors`
    /// is off (`Z/main/main.c:1453-1454`):
    ///
    /// ```php
    /// foreach (['display_startup_errors', 'display_errors'] as $off) {
    ///     $p = proc_open([PHP_BINARY, '-n', '-d', 'disable_functions=exit', '-d', "$off=0", '-r', 'echo 1;'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    ///     var_dump(stream_get_contents($pipes[1]), stream_get_contents($pipes[2])); proc_close($p);
    /// }
    /// ```
    /// ```expect process
    /// string(1) "1"
    /// string(66) "PHP Warning:  Cannot disable function exit() in Unknown on line 0
    /// "
    /// string(1) "1"
    /// string(66) "PHP Warning:  Cannot disable function exit() in Unknown on line 0
    /// "
    /// ```
    fn log_errors(&self) -> bool {
        let off = |name: &[u8]| self.ini().get(name).is_some_and(|v| !Ini::parse_bool(v));
        self.ini_flag(b"log_errors")
            || (self.diagnostics.display_direct
                && (off(b"display_startup_errors")
                    || display_errors_mode_of(self.ini().get(b"display_errors")) == 0))
    }

    /// `determine_charset(NULL, quiet)` (`Z/ext/standard/html.c:364`),
    /// the charset [`escape_html`] reads a message in: the name
    /// `get_default_charset` gives, `internal_encoding` where it is set
    /// and `default_charset` otherwise, each as php's C string reads it,
    /// to its first NUL, and UTF-8 where it names none `charset_map`
    /// takes.
    pub(crate) fn html_charset(&self) -> Charset {
        [b"internal_encoding".as_slice(), b"default_charset"]
            .iter()
            .find_map(|name| {
                let value = self.ini().get(name)?;
                let named = value.split(|&byte| byte == 0).next().unwrap_or_default();
                (!named.is_empty()).then_some(named)
            })
            .and_then(Charset::named)
            .unwrap_or(Charset::Utf8)
    }

    /// `display_errors`, php's three values: off, the body, and the
    /// host's error stream, which the hosts php names at
    /// `Z/main/main.c:1484`, `cli`, `cgi` and `phpdbg`, write a
    /// diagnostic to without php's leading newline. Under any other
    /// host `stderr` is the body, since php's test is on
    /// `sapi_module.name` and the plain form is its `else`.
    ///
    /// The value is read as `php_get_display_errors_mode`
    /// (`Z/main/main.c:492-517`) reads it: no entry, `on`, `yes`,
    /// `true` and `stdout` are `PHP_DISPLAY_ERRORS_STDOUT`, `stderr` is
    /// `PHP_DISPLAY_ERRORS_STDERR`, and any other value is `ZEND_ATOL`
    /// of it narrowed to php's `uint8_t`, where `2` is the error stream,
    /// `0` is off and every other number is `STDOUT`. The narrowing is
    /// php's and observable, `256` is off and `258` the error stream,
    /// so it is copied.
    pub(super) fn display_errors(&self) -> Display {
        // `php_error_cb`'s display branch (`Z/main/main.c:1469`): while
        // modules start and while the request starts, what is raised is
        // displayed only under `display_startup_errors`, php's default on.
        let starting = self.diagnostics.display_direct || self.phase() == Phase::Startup;
        if starting
            && self
                .ini()
                .get(b"display_startup_errors")
                .is_some_and(|value| !Ini::parse_bool(value))
        {
            return Display::Off;
        }
        let mode = self
            .diagnostics
            .display_errors_word
            .unwrap_or_else(|| display_errors_mode_of(self.ini().get(b"display_errors")));
        match mode {
            0 => Display::Off,
            DISPLAY_ERRORS_STDERR if self.host_writes_stderr() => Display::Stderr,
            _ => Display::Body,
        }
    }

    /// Whether the host is one php writes `display_errors=stderr` to
    /// the error stream under.
    pub(super) fn host_writes_stderr(&self) -> bool {
        matches!(
            self.description.sapi_name.as_str(),
            "cli" | "cgi" | "phpdbg"
        )
    }

    /// Enter php's startup on this request: what is raised from here is
    /// held, not shown ([`Request::end_startup`]).
    pub(crate) fn begin_startup(&mut self) {
        self.diagnostics.startup = Some(Vec::new());
        self.set_ini_stage(IniStage::Startup);
    }

    /// How many diagnostics the startup has raised so far.
    pub(crate) fn startup_raised_len(&self) -> usize {
        self.diagnostics.startup.as_ref().map_or(0, Vec::len)
    }

    /// Leave the startup: what was raised during it, in order.
    pub(crate) fn end_startup(&mut self) -> Vec<(Level, Vec<u8>)> {
        self.set_ini_stage(IniStage::Runtime);
        self.diagnostics.startup.take().unwrap_or_default()
    }

    /// Whether a handler left a throwable pending, php's `EG(exception)`:
    /// the test the loop performs after a `throw`-effect op and after a
    /// `Flow::Resume` (`jit-design.md` §2.2). The unwind exit is one
    /// answer, since it stands in php's slot as any throwable does;
    /// [`Request::pending`] is the object alone.
    /// `zend_replace_error_handling`: the class a warning is thrown as,
    /// `None` for `EH_NORMAL`; the one it replaces comes back.
    pub(crate) fn replace_error_handling(&mut self, class: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.diagnostics.error_handling, class)
    }

    /// The throwable a warning became under `EH_THROW`, taken.
    pub(crate) fn take_error_handling_thrown(&mut self) -> Option<Thrown> {
        self.diagnostics.error_handling_thrown.take()
    }

    /// What [`last_error`](Self::last_error) makes, its one owner not yet named.
    pub(crate) fn last_error_made(&mut self) -> Result<Option<PhpArray>, Thrown> {
        let Some(last) = self.diagnostics.last_error else {
            return Ok(None);
        };
        let mut array = self.array()?;
        // `ZVAL_STR_COPY` of the message and the file.
        for (name, value) in [
            (&b"type"[..], Value::int(last.level.bit())),
            (b"message", Value::string(last.message)),
            (b"file", Value::string(last.file)),
            (b"line", Value::int(i64::from(last.line))),
        ] {
            // `ZSTR_KNOWN`, the four names permanent.
            let key = self.declared_string(name)?.into_key();
            array.set(self, key, Owner::take(self, value))?;
        }
        if let Some((Some(trace), _)) = self.diagnostics.last_fatal_error_backtrace {
            let key = self.declared_string(b"trace")?.into_key();
            array.set(self, key, Owner::take(self, Value::array(trace)))?;
        }
        Ok(Some(array))
    }

    /// The last error where its level is one of `E_FATAL_ERRORS`, its
    /// message, file and line.
    pub(crate) fn last_fatal(&self) -> Option<(String, Vec<u8>, u32)> {
        let LastError {
            level,
            message,
            file,
            line,
        } = self.diagnostics.last_error?;
        let message = String::from_utf8_lossy(message.as_bytes()).into_owned();
        let file = file.as_bytes().to_vec();
        fatal_errors(level).then_some((message, file, line))
    }

    /// `clear_last_error` (`Z/main/main.c:1307`): the message and the
    /// file given back. php's request end calls it from
    /// `php_free_request_globals`.
    pub(crate) fn clear_last_error_strings(&mut self) {
        if let Some(last) = self.diagnostics.last_error.take() {
            self.release(Value::string(last.message));
            self.release(Value::string(last.file));
        }
    }

    /// `EG(error_reporting) = level`, the store `BEGIN_SILENCE` and
    /// `END_SILENCE` make and the unwinder makes for a `ZEND_LIVE_SILENCE`
    /// range: the word alone, the ini entry left where it stands.
    pub(crate) fn set_error_reporting(&mut self, level: i64) {
        self.diagnostics.error_reporting = level;
    }

    /// The write `error_reporting(int $error_level)` makes
    /// (`Z/Zend/zend_builtin_functions.c:470-497`): the entry's value
    /// replaced by the number's decimal text, `zend_long_to_str`, past
    /// its `on_modify`, and the word set to the number.
    ///
    /// The entry's value is the request string `zend_long_to_str` makes,
    /// and the one it held before is released, so a change after the
    /// table exists costs the string:
    ///
    /// ```php
    /// ini_set('precision', '14');
    /// $m = memory_get_usage(); error_reporting(30711); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); error_reporting(-1); error_reporting(30719); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// 0
    /// ```
    pub(crate) fn alter_error_reporting(&mut self, level: i64) {
        let text = level.to_string();
        // The memory limit's fatal is recorded and ends the request.
        let string = self.string(text.as_bytes()).ok();
        self.ini_hold_string(b"error_reporting", Some(text.as_bytes()), string);
        self.diagnostics.error_reporting = level;
    }

    /// `OnUpdateErrorReporting` (`Z/Zend/zend.c:111`): the word read out
    /// of the `error_reporting` entry, `atoi` of its text, and `E_ALL`
    /// where the entry has no value. Made at request start and by
    /// `ini_set('error_reporting', ...)`.
    pub(super) fn update_error_reporting(&mut self) {
        self.diagnostics.error_reporting = self.ini_error_reporting();
    }

    /// `INI_INT("error_reporting")`, `E_ALL` where the entry has no value
    /// (`OnUpdateErrorReporting`), php's `NULL`, the default, and the
    /// empty string's `atoi`, 0, where it was set empty: the level the
    /// entry says, which is what a fiber starts
    /// its body at (`zend_fiber_execute`), whatever an `@` around the
    /// `start` masked.
    pub(crate) fn ini_error_reporting(&self) -> i64 {
        self.ini().value(b"error_reporting").map_or(E_ALL, strtol)
    }

    /// `ZEND_FUNCTION(restore_error_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1332`): the handler standing
    /// released and the top of the stack standing in its place with its
    /// mask; with the stack empty, no handler and the mask where it was.
    /// php answers `true` whatever it found.
    pub(crate) fn restore_error_handler(&mut self) -> bool {
        if let Some(handler) = self.diagnostics.error_handler.take() {
            self.release(handler.into_value());
        }
        if let Some((handler, levels)) = self.diagnostics.error_handlers.pop() {
            self.diagnostics.error_handler = handler;
            self.diagnostics.error_handler_levels = levels;
        }
        true
    }

    /// `EG(user_error_handler)` as `get_error_handler()` reads it: the
    /// value the handler was set from, a view, none while no handler
    /// stands.
    pub(crate) fn error_handler(&self) -> Option<Value> {
        self.diagnostics.error_handler.as_ref().map(Callable::value)
    }

    /// `ZEND_FUNCTION(set_exception_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1367`): the error handler's
    /// shape without a mask.
    pub(crate) fn set_exception_handler(&mut self, handler: Option<Callable>) -> Option<Value> {
        let previous = self.diagnostics.exception_handler.take();
        let answer = previous.as_ref().map(Callable::value);
        self.diagnostics.exception_handlers.push(previous);
        if let Some(handler) = handler {
            self.addref(&handler.value());
            self.diagnostics.exception_handler = Some(handler);
        }
        answer
    }

    /// `ZEND_FUNCTION(restore_exception_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1399`).
    pub(crate) fn restore_exception_handler(&mut self) -> bool {
        if let Some(handler) = self.diagnostics.exception_handler.take() {
            self.release(handler.into_value());
        }
        if let Some(handler) = self.diagnostics.exception_handlers.pop() {
            self.diagnostics.exception_handler = handler;
        }
        true
    }

    /// `EG(user_exception_handler)` as `get_exception_handler()` reads
    /// it, a view.
    pub(crate) fn exception_handler(&self) -> Option<Value> {
        self.diagnostics
            .exception_handler
            .as_ref()
            .map(Callable::value)
    }

    /// `zend_shutdown_executor_values`' release of both handlers and
    /// both stacks (`Z/Zend/zend_execute_API.c:410-420`), and SPL's
    /// `RSHUTDOWN` of the autoload stack beside them, after the open
    /// resources are closed, so a resource's destructor that raises
    /// still reaches the handler that stood.
    pub(crate) fn free_user_handlers(&mut self) {
        let error = self.diagnostics.error_handler.take().into_iter().chain(
            std::mem::take(&mut self.diagnostics.error_handlers)
                .into_iter()
                .filter_map(|(handler, _)| handler),
        );
        let exception = self.diagnostics.exception_handler.take().into_iter().chain(
            std::mem::take(&mut self.diagnostics.exception_handlers)
                .into_iter()
                .flatten(),
        );
        let handlers: Vec<Callable> = error.chain(exception).collect();
        for handler in handlers {
            self.release(handler.into_value());
        }
        self.free_autoloaders();
        // `PHP_RSHUTDOWN(basic)`'s `zend_llist_destroy` of
        // `BG(user_tick_functions)`, and `SG(callback_func)` where the
        // header block was never sent.
        self.free_tick_functions();
        if let Some(callback) = self.take_header_callback() {
            callback.give_up(self);
        }
    }

    /// `zend_error_impl`'s user branch (`Z/Zend/zend.c:1527-1604`): the
    /// handler standing is handed the diagnostic where its mask has the
    /// level, and where the level is not one php holds unsafe to hand to
    /// user code, `E_ERROR`, `E_PARSE`, the two core levels and the two
    /// compile levels. The handler is taken down while it runs, so a
    /// diagnostic it raises reaches the built-in handler, and put back
    /// after unless it set one of its own. It is handed the level, the
    /// message, the file and the line; `true` says the diagnostic is
    /// served, which is every answer but `false` and a throw, which is
    /// left pending in the diagnostic's place. With a throwable already
    /// pending, php's `zend_call_function` returns before the call and
    /// the diagnostic is dropped, handled by nobody.
    ///
    /// The file it is handed is the op array's file name, one more owner
    /// of the interned string, so a handler's call, and a throw from it,
    /// peaks where php's does whatever the path's length:
    ///
    /// ```php
    /// set_error_handler(function ($n, $s) { throw new Exception($s); });
    /// function f() { try { trigger_error('Use of "parent" in callables is deprecated', E_USER_DEPRECATED); } catch (Exception $e) {} }
    /// f(); $s = memory_get_usage(); memory_reset_peak_usage(); f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n";
    /// ```
    /// ```expect
    /// 2312 32
    /// ```
    fn user_error_handler(
        &mut self,
        level: Level,
        message: PhpString,
        file: &[u8],
        line: u32,
    ) -> bool {
        if self.diagnostics.error_handler.is_none()
            || self.diagnostics.error_handler_levels & level.bit() == 0
            || matches!(
                level,
                Level::Error
                    | Level::Parse
                    | Level::CoreError
                    | Level::CoreWarning
                    | Level::CompileError
                    | Level::CompileWarning
            )
        {
            return false;
        }
        if self.is_pending() {
            return true;
        }
        let Some(handler) = self.diagnostics.error_handler.take() else {
            return false;
        };
        let arguments = (|| -> Result<[Value; 4], Thrown> {
            // `ZVAL_STR_COPY(&params[2], error_filename)`: the op array's
            // file name, one more owner of it as a trace row takes.
            let file = self.declared_string(file)?.into_value().hand_on();
            // `ZVAL_STR_COPY(&params[1], message)`.
            self.addref(&Value::string(message));
            Ok([
                Value::int(level.bit()),
                Value::string(message),
                file,
                Value::int(i64::from(line)),
            ])
        })();
        let served = match arguments {
            Ok(arguments) => {
                let record = self.record_set_aside();
                let answer = crate::api::call::call_made(self, &handler, &arguments);
                // A handler that bailed out jumped past the line that puts
                // the record back, which stays off and empty.
                if !self.diagnostics().bailing_out() {
                    self.record_restored(record);
                }
                for argument in arguments {
                    self.release(argument);
                }
                match answer {
                    Ok(value) => {
                        let fall_through = matches!(value.as_ref(), ValueRef::Bool(false));
                        self.release(value);
                        !fall_through
                    }
                    Err(thrown) => {
                        self.leave_pending(&thrown);
                        true
                    }
                }
            }
            Err(thrown) => {
                self.leave_pending(&thrown);
                true
            }
        };
        if self.diagnostics.error_handler.is_none() {
            self.diagnostics.error_handler = Some(handler);
        } else {
            self.release(handler.into_value());
        }
        served
    }

    /// `zend_user_exception_handler` (`Z/Zend/zend.c:1931`): the pending
    /// throwable handed to the handler standing, which is taken down
    /// onto the stack while it runs and stands again after unless it
    /// set one of its own. The throwable is released once the handler
    /// returns. What the handler throws, or an `exit` in it, is
    /// rendered as the uncaught fatal by `zend_call_function`'s tail,
    /// since no frame and no handler stands to take it, and the request
    /// leaves by `zend_bailout`, the `Break` here: php's jump skips the
    /// rest of this function, so the handler stays taken down, and a
    /// throwable raised with no frame after it is rendered uncaught
    /// (`ext/zend_test`'s throwing resource is the witness). Nothing is
    /// done for the unwind exit.
    ///
    /// ```php
    /// set_exception_handler(function ($e) { echo "handler: ", $e->getMessage(), "\n"; throw new Exception("h"); });
    /// register_shutdown_function(function () { var_dump(get_exception_handler()); });
    /// throw new Exception("main");
    /// ```
    /// ```expect exit=255
    /// handler: main
    ///
    /// Fatal error: Uncaught Exception: h in %s:2
    /// Stack trace:
    /// #0 [internal function]: {closure:%s:2}(Object(Exception))
    /// #1 {main}
    ///   thrown in %s on line 2
    /// NULL
    /// ```
    pub(crate) fn user_exception_handler(&mut self) -> ControlFlow<()> {
        if self.diagnostics.exception_handler.is_none() {
            return ControlFlow::Continue(());
        }
        let Some(throwable) = self.take_pending_view() else {
            return ControlFlow::Continue(());
        };
        let handler = self.diagnostics.exception_handler.take();
        self.diagnostics.exception_handlers.push(handler.clone());
        if let Some(handler) = handler {
            match crate::api::call::call_made(self, &handler, &[throwable.as_value()]) {
                Ok(value) => self.release(value),
                Err(thrown) => {
                    // A frame standing hands the throwable back rather
                    // than render it, which php never meets here; it is
                    // rendered as the tail would have.
                    if thrown.exception().is_some() {
                        self.leave_pending(&thrown);
                        self.exception_error(Level::Error).ok();
                    }
                    self.release(throwable.as_value());
                    return ControlFlow::Break(());
                }
            }
        }
        self.release(throwable.as_value());
        if self.diagnostics.exception_handler.is_none()
            && let Some(top) = self.diagnostics.exception_handlers.pop()
        {
            self.diagnostics.exception_handler = top;
        }
        ControlFlow::Continue(())
    }
}

impl Diagnostics<&Request> {
    /// `EG(error_reporting)` as it stands: what `error_reporting()`
    /// answers and what every diagnostic is masked by.
    #[must_use]
    pub fn error_reporting(self) -> i64 {
        self.0.diagnostics.error_reporting
    }
}

impl Diagnostics<&mut Request> {
    /// `PG(display_errors) = word`, the word written past the ini
    /// table, as `SOAP_SERVER_BEGIN_CODE` turns diagnostics off around a
    /// call (`Z/ext/soap/soap.c:1909-1959`): no entry is altered, so
    /// nothing joins `EG(modified_ini_directives)`, no string is held and
    /// `ini_get('display_errors')` reads as before. The word in force
    /// before, read as `php_get_display_errors_mode` reads the entry, is
    /// returned for the caller to write back.
    pub fn set_display_errors(self, word: u8) -> u8 {
        let before = self
            .0
            .diagnostics
            .display_errors_word
            .unwrap_or_else(|| display_errors_mode_of(self.0.ini().get(b"display_errors")));
        self.0.diagnostics.display_errors_word = Some(word);
        before
    }

    /// `error_get_last()`: the last diagnostic as php's four-key array,
    /// its strings the request's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn last_error(self) -> Result<Option<Owner<PhpArray>>, Thrown> {
        let made = self.0.last_error_made();
        adopted_some(made)
    }

    /// `error_clear_last()`.
    pub fn clear_last_error(self) {
        self.0.clear_last_error_strings();
        self.0.clear_fatal_error_backtrace();
    }

    /// `error_log()` to the host's log: `php_log_err` with the message
    /// as given, one entry.
    pub fn error_log(self, message: &[u8]) {
        self.0.diagnostics_mut().log_err_with_severity(message, 5);
    }

    /// `php_log_err_with_severity`: `message` logged as one entry, as
    /// [`Diagnostics::error_log`] logs it, at `priority`, the `syslog(3)`
    /// level `LOG_EMERG` (0) to `LOG_DEBUG` (7) a [`Syslog`] is handed
    /// where `error_log` is `syslog`; `php_log_err` is this at
    /// `LOG_NOTICE` (5). An entry asked for while one is being logged
    /// is dropped, `PG(in_error_log)`'s guard.
    pub fn log_err_with_severity(self, message: &[u8], priority: i32) {
        let _ = self.0.log_err(message, priority);
    }

    /// `body` called with `EG(error_reporting)` lowered by `levels`, and the
    /// word as it stood put back after, whatever `body` set it to: what
    /// `php_http.c`'s `http_connect` does around a connect, so a masked
    /// diagnostic is not shown and still reaches `error_get_last()` and a
    /// user's handler.
    pub fn reporting_without<T>(self, levels: &[Level], body: impl FnOnce(&mut Request) -> T) -> T {
        let standing = self.0.diagnostics().error_reporting();
        let masked = levels.iter().fold(0, |m, level| m | level.bit());
        self.0.set_error_reporting(standing & !masked);
        let answer = body(self.0);
        self.0.set_error_reporting(standing);
        answer
    }

    /// `ZEND_FUNCTION(set_error_handler)`
    /// (`Z/Zend/zend_builtin_functions.c:1306`): the handler standing is
    /// pushed with its mask, `zend_stack_push` of both, and `handler`
    /// stands with `levels` as its mask; none leaves no handler and the
    /// mask where it was. The answer is the handler that stood, a view
    /// the stack keeps the owner of, which the native copies. The
    /// handler given is one more owner from here.
    pub fn set_error_handler(self, handler: Option<Callable>, levels: i64) -> Option<Value> {
        let previous = self.0.diagnostics.error_handler.take();
        let answer = previous.as_ref().map(Callable::value);
        self.0
            .diagnostics
            .error_handlers
            .push((previous, self.0.diagnostics.error_handler_levels));
        if let Some(handler) = handler {
            self.0.addref(&handler.value());
            self.0.diagnostics.error_handler = Some(handler);
            self.0.diagnostics.error_handler_levels = levels;
        }
        answer
    }
}

/// Where `display_errors` sends a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Display {
    Off,
    Body,
    Stderr,
}

/// `PHP_DISPLAY_ERRORS_STDOUT` and `PHP_DISPLAY_ERRORS_STDERR`
/// (`Z/main/php_globals.h:36-37`), the two modes of `PG(display_errors)`
/// beside zero.
const DISPLAY_ERRORS_STDOUT: u8 = 1;

pub(super) const DISPLAY_ERRORS_STDERR: u8 = 2;

/// `php_get_display_errors_mode` (`Z/main/main.c:492-517`), as
/// [`Request::display_errors`] reads it.
pub(super) fn display_errors_mode_of(value: Option<&[u8]>) -> u8 {
    match value {
        None => DISPLAY_ERRORS_STDOUT,
        Some(value) => match value.to_ascii_lowercase().as_slice() {
            b"on" | b"yes" | b"true" | b"stdout" => DISPLAY_ERRORS_STDOUT,
            b"stderr" => DISPLAY_ERRORS_STDERR,
            _ => display_errors_mode(strtol(value)),
        },
    }
}

/// `uint8_t mode = ZEND_ATOL(...)` at `Z/main/main.c:513`: the number
/// narrowed to php's byte.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "php's `uint8_t mode = ZEND_ATOL(value)`, whose narrowing is the behaviour being copied"
)]
fn display_errors_mode(number: i64) -> u8 {
    number as u8
}

/// `E_ALL` (`Z/Zend/zend_errors.h:43`), the fourteen levels php 8.5
/// raises, which is every bit below `E_USER_DEPRECATED`'s but
/// `E_STRICT`'s, the one 8.4 removed from the sum.
const E_ALL: i64 = 32767 & !Level::Strict.bit();

/// php's `error_type_str` (`Z/main/main.c:1414-1448`), the word both
/// renderings name the level by. `E_RECOVERABLE_ERROR` is the one level
/// whose word is not its family's, and `E_STRICT` is the one level php
/// 8.5 has no case for, so it falls to the `default:` every level php
/// dropped falls to.
///
/// No fence of this crate reaches either word, so the two arms are held
/// by the test below. php 8.5 raises `E_RECOVERABLE_ERROR` at one site,
/// `zend_object_is_true` (`Z/Zend/zend_operators.c:2918`), when an
/// object's `cast_object` handler refuses `_IS_BOOL`; the standard
/// handler answers `true` for every user class, and of the bundle only
/// `ext/zend_test`'s classes bring a handler that refuses, so the word
/// is witnessed there, and `trigger_error()` takes the four `E_USER_*`
/// levels alone.
/// `E_STRICT` has no raise anywhere: its constant stands `#[Deprecated]`
/// since 8.4 with the message that the level was removed.
/// `E_FATAL_ERRORS` (`Z/Zend/zend_errors.h:47`): the six levels
/// `php_error_cb` bails out of, and the ones `zend_error_impl` fetches a
/// trace for under `fatal_error_backtraces`.
pub(super) fn fatal_errors(level: Level) -> bool {
    level.bit() & crate::exec::E_FATAL_ERRORS != 0
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error | Level::CoreError | Level::CompileError | Level::UserError => "Fatal error",
        Level::RecoverableError => "Recoverable fatal error",
        Level::Warning | Level::CoreWarning | Level::CompileWarning | Level::UserWarning => {
            "Warning"
        }
        Level::Parse => "Parse error",
        Level::Notice | Level::UserNotice => "Notice",
        Level::Deprecated | Level::UserDeprecated => "Deprecated",
        Level::Strict => "Unknown error",
    }
}

/// What php's `escape_html` (`Z/main/main.c:1038-1050`) leaves of a
/// message: the escaped bytes, and each size `php_escape_html_entities_ex`
/// asked for in the pass that wrote them, the string it grows as it
/// writes (`Z/ext/standard/html.c:1173-1194`): `zend_string_alloc(maxlen)`
/// with `maxlen` 128 for a message under 64 bytes and twice its length
/// otherwise, then each `zend_string_safe_realloc` by 128 before a
/// character that finds fewer than 40 bytes left, every one
/// `_ZSTR_STRUCT_SIZE(maxlen)`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct EscapedHtml {
    pub(crate) bytes: Vec<u8>,
    pub(crate) sizes: Vec<usize>,
}

/// php's `escape_html` (`Z/main/main.c:1038-1050`): the message of an
/// `E_ERROR` or an `E_PARSE`, and `php_verror`'s message and origin,
/// through `php_escape_html_entities_ex` at `ENT_COMPAT` with
/// `double_encode`, in the charset `determine_charset(NULL)` finds
/// ([`Request::html_charset`]): `htmlspecialchars`'s four, `&`, `<`, `>`
/// and `"`, an already-written entity escaped again, `'` left alone,
/// and every other character's bytes as they stand. A message that is
/// not that charset comes back empty from the first pass, and the
/// second substitutes each ill-formed sequence, U+FFFD in UTF-8 and
/// `&#xFFFD;` in any other charset.
///
/// So an uncaught throwable's message in a single-byte charset keeps its
/// bytes, php's own `bug74725.phpt`; the `ob_start()` callback spells
/// out the two bytes the fence could not hold:
///
/// ```php
/// ini_set('html_errors', '1');
/// ini_set('default_charset', 'Windows-1251');
/// ob_start(fn ($out) => strtr($out, ["\xF2" => '\xF2', "\xE5" => '\xE5']));
/// throw new Exception("\xF2\xE5");
/// ```
/// ```expect exit=255
/// <br />
/// <b>Fatal error</b>:  Uncaught Exception: \xF2\xE5 in %s:5
/// Stack trace:
/// #0 {main}
///   thrown in <b>%s</b> on line <b>5</b><br />
/// ```
///
/// and the same bytes are not UTF-8, so `php_verror`'s origin, escaped
/// the same way, has each substituted:
///
/// ```php
/// ini_set('html_errors', '1');
/// ob_start(fn ($out) => strtr($out, ["\u{FFFD}" => '\u{FFFD}']));
/// @fopen("\xF2\xE5<", 'r');
/// echo error_get_last()['message'], "\n";
/// ```
/// ```expect
/// fopen(\u{FFFD}\u{FFFD}&lt;): Failed to open stream: No such file or directory
/// ```
pub(crate) fn escape_html(message: &[u8], charset: Charset) -> EscapedHtml {
    escape_pass(message, charset, false)
        .filter(|escaped| !escaped.bytes.is_empty())
        .or_else(|| escape_pass(message, charset, true))
        .unwrap_or_default()
}

/// One pass of `php_escape_html_entities_ex` over `message` at
/// `ENT_COMPAT` with `double_encode`: `None` where a character is
/// ill-formed and `substitute`, `ENT_HTML_SUBSTITUTE_ERRORS`, is not
/// set, which php answers with the empty string.
fn escape_pass(message: &[u8], charset: Charset, substitute: bool) -> Option<EscapedHtml> {
    // `_ZSTR_STRUCT_SIZE`: the header, the bytes and the terminator.
    let size = |maxlen: usize| 24 + maxlen + 1;
    let mut maxlen = if message.len() < 64 {
        128
    } else {
        message.len() * 2
    };
    let replacement: &[u8] = if charset == Charset::Utf8 {
        b"\xEF\xBF\xBD"
    } else {
        b"&#xFFFD;"
    };
    let mut escaped = EscapedHtml {
        bytes: Vec::with_capacity(message.len()),
        sizes: vec![size(maxlen)],
    };
    let mut cursor = 0;
    while cursor < message.len() {
        let before = cursor;
        let (letter, next) = charset.next_char(message, cursor);
        cursor = next;
        if escaped.bytes.len() > maxlen - 40 {
            maxlen += 128;
            escaped.sizes.push(size(maxlen));
        }
        // `find_entity_for_char_basic`: the basic table holds the code
        // points under 64 alone, so a multi-byte code unit is passed
        // through whatever its bytes are.
        let entity: &[u8] = match letter {
            None if substitute => replacement,
            None => return None,
            Some(0x26) => b"&amp;",
            Some(0x3C) => b"&lt;",
            Some(0x3E) => b"&gt;",
            Some(0x22) => b"&quot;",
            Some(_) => &message[before..cursor],
        };
        escaped.bytes.extend_from_slice(entity);
    }
    Some(escaped)
}

/// `smart_str_append_escaped` (`Z/Zend/zend_smart_str.c:77`): a byte
/// under 32, over 126 or a backslash written as `\n`, `\r`, `\t`, `\f`,
/// `\v`, `\\`, `\e` or `\xNN` with the digits in upper case, and every
/// other byte as itself. What a string argument in a trace and
/// `zend_print_zval_r`'s cousins write.
pub(crate) fn append_escaped(out: &mut String, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x0c => out.push_str("\\f"),
            0x0b => out.push_str("\\v"),
            b'\\' => out.push_str("\\\\"),
            0x1b => out.push_str("\\e"),
            byte if !(32..=126).contains(&byte) => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push_str("\\x");
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0xf)]));
            }
            byte => out.push(char::from(byte)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::response::Aside;
    use super::{Charset, append_escaped, escape_html, level_name};
    use crate::api::raising::Level;
    use crate::engine::Engine;
    use crate::test::request_on;

    /// A refusal php's compiler throws, uncaught in the program, is
    /// rendered by `zend_exception_error` with no fatal's trace after it
    /// whatever `fatal_error_backtraces` says, where one it raises takes
    /// the trace; the recipe turns the entry off, so the witness on
    /// `refused_program` cannot tell the two apart and this does.
    #[test]
    fn a_thrown_refusal_takes_no_fatal_trace_and_a_raised_one_does() {
        let served = |script: &str| {
            let out = Engine::cli_builder()
                .ini("fatal_error_backtraces", "1")
                .build()
                .serve_capture(script, crate::test::Capture::default());
            out.stdout_text()
        };
        let thrown = served("<?php\ndeclare(encoding = M_PI);");
        assert!(
            thrown.contains("Fatal error: Encoding must be a literal"),
            "{thrown}"
        );
        assert!(!thrown.contains("Stack trace"), "{thrown}");
        let raised = served("<?php\necho 1;\ndeclare(encoding = 'UTF-8');");
        assert!(raised.contains("very first statement"), "{raised}");
        assert!(raised.contains("Stack trace"), "{raised}");
    }

    /// `smart_str_append_escaped`'s table, one byte per arm: the seven
    /// named escapes, the hex form for the rest of the control range and
    /// everything past ASCII, and a printable byte untouched.
    #[test]
    fn a_trace_escapes_a_string_as_smart_str_does() {
        let mut out = String::new();
        append_escaped(&mut out, b"a\n\r\t\x0c\x0b\\\x1b\x01\x7f\xff'z");
        assert_eq!(out, "a\\n\\r\\t\\f\\v\\\\\\e\\x01\\x7F\\xFF'z");
    }

    /// php's `error_type_str` for every level `Level` names, the switch
    /// at `Z/main/main.c:1414-1448` read case by case. No PHP source
    /// raises `E_RECOVERABLE_ERROR` or `E_STRICT` under php 8.5, as the
    /// rustdoc beside `level_name` says, so this is the one place the
    /// two words are held.
    #[test]
    fn every_level_is_named_as_php_names_it() {
        for (level, name) in [
            (Level::Error, "Fatal error"),
            (Level::CoreError, "Fatal error"),
            (Level::CompileError, "Fatal error"),
            (Level::UserError, "Fatal error"),
            (Level::RecoverableError, "Recoverable fatal error"),
            (Level::Warning, "Warning"),
            (Level::CoreWarning, "Warning"),
            (Level::CompileWarning, "Warning"),
            (Level::UserWarning, "Warning"),
            (Level::Parse, "Parse error"),
            (Level::Notice, "Notice"),
            (Level::UserNotice, "Notice"),
            (Level::Deprecated, "Deprecated"),
            (Level::UserDeprecated, "Deprecated"),
            (Level::Strict, "Unknown error"),
        ] {
            assert_eq!(level_name(level), name, "{level:?}");
        }
    }

    /// What php's `escape_html` writes, from the pinned oracle under the
    /// witness recipe with `-d html_errors=1`: `<?php\necho <;` is a
    /// parse error whose message names the token it did not want, and
    /// php writes `syntax error, unexpected token &quot;&lt;&quot;`.
    /// `htmlspecialchars($s, ENT_COMPAT)` over `<a&b'c"d` under the same
    /// oracle is `&lt;a&amp;b'c&quot;d`, which is the whole set: the
    /// apostrophe `ENT_QUOTES` would take stands as it is.
    #[test]
    fn the_message_of_a_fatal_is_escaped_as_php_escapes_it() {
        let utf8 = |message: &str| escape_html(message.as_bytes(), Charset::Utf8).bytes;
        assert_eq!(
            utf8("syntax error, unexpected token \"<\""),
            b"syntax error, unexpected token &quot;&lt;&quot;"
        );
        assert_eq!(utf8("<a&b'c\"d"), b"&lt;a&amp;b'c&quot;d");
        assert_eq!(
            utf8("&amp; &gt;"),
            b"&amp;amp; &amp;gt;",
            "php passes double_encode, so a written entity is written again"
        );
        assert_eq!(utf8("nothing to escape"), b"nothing to escape");
    }

    /// `htmlspecialchars($s, ENT_COMPAT | ENT_SUBSTITUTE, $charset)`
    /// under the pinned oracle, which is `escape_html`'s second pass: a
    /// message valid in its charset keeps its bytes, and one that is not
    /// has each ill-formed sequence substituted.
    #[test]
    fn a_message_keeps_its_bytes_in_its_charset() {
        let escaped = |message: &[u8], charset| escape_html(message, charset).bytes;
        assert_eq!(escaped(b"\xF2\xE5<", Charset::Cp1251), b"\xF2\xE5&lt;");
        assert_eq!(
            escaped(b"\xF2\xE5<", Charset::Utf8),
            b"\xEF\xBF\xBD\xEF\xBF\xBD&lt;"
        );
        assert_eq!(
            escaped(b"\x82\xA0\x82<", Charset::Sjis),
            b"\x82\xA0&#xFFFD;&lt;"
        );
        assert_eq!(escaped(b"", Charset::Utf8), b"");
    }

    /// `(EG(error_reporting) & type) || (type & E_CORE)`: a core level
    /// is rendered under `error_reporting=0`, in the same word its
    /// family takes, and every other level is masked. The one raiser
    /// that reaches `E_CORE_ERROR` is [`super::Request::save_pending`],
    /// which no PHP snippet reaches, and no raiser reaches
    /// `E_CORE_WARNING`, so the mask is held here and not by a witness.
    /// `E_CORE_ERROR` is in `E_FATAL_ERRORS`, so under php's default
    /// `fatal_error_backtraces` it fetches its trace, `#0 {main}` with no
    /// frame standing, and the warning fetches none.
    #[test]
    fn a_core_level_is_rendered_past_error_reporting() {
        let engine = Engine::cli_builder().ini("error_reporting", "0").build();
        let file = b"witness.php";
        let mut request = request_on(&engine);
        request.note_error_in(Level::CoreWarning, "core", file, 2);
        request.note_error_in(Level::CoreError, "core", file, 3);
        request.note_error_in(Level::Warning, "masked", file, 4);
        request.note_error_in(Level::Error, "masked", file, 5);
        assert_eq!(
            String::from_utf8_lossy(&request.take_body()),
            "\nWarning: core in witness.php on line 2\n\nFatal error: core in witness.php on line 3\nStack trace:\n#0 {main}\n"
        );
    }

    /// php's second html format (`Z/main/main.c:1475`), the one every
    /// fatal level but `E_ERROR` and `E_PARSE` takes: the message as it
    /// is, and the trace between the `<br />` and the newline. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\nini_set('fatal_error_backtraces', '1'); trigger_error('b<o>om', E_USER_ERROR);`
    /// is the 8.4 deprecation and then
    /// `<br />\n<b>Fatal error</b>:  b<o>om in <b>%s</b> on line <b>2</b><br />\nStack trace:\n#0 %s(2): trigger_error('b<o>om', 256)\n#1 {main}\n`;
    /// no native here raises `E_USER_ERROR`, so the request has no frame
    /// and the trace is `{main}` alone. The same level under
    /// `fatal_error_backtraces=0` fetches none, and the two formats then
    /// write the same bytes but for the escaping.
    #[test]
    fn the_second_html_format_carries_the_trace_after_the_line() {
        let file = b"witness.php";
        let engine = Engine::cli_builder().ini("html_errors", "1").build();
        let mut req = request_on(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        req.note_error_in(Level::Error, "b<o>om", file, 3);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\nStack trace:\n#0 {main}\n\
             <br />\n<b>Fatal error</b>:  b&lt;o&gt;om in <b>witness.php</b> on line <b>3</b><br />\n"
        );

        let engine = Engine::cli_builder()
            .ini("html_errors", "1")
            .ini("fatal_error_backtraces", "0")
            .build();
        let mut req = request_on(&engine);
        req.note_error_in(Level::UserError, "b<o>om", file, 2);
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<br />\n<b>Fatal error</b>:  b<o>om in <b>witness.php</b> on line <b>2</b><br />\n"
        );
    }

    /// The log form and the xml fault carry the trace after the site as
    /// the plain form does, `ZSTR_LEN(backtrace) ? "\nStack trace:\n" : ""`
    /// in each of `php_error_cb`'s `spprintf` calls. What the pinned
    /// oracle writes for `-d log_errors=1 -d display_errors=0` over
    /// `<?php\n(unset)$x;` is
    /// `PHP Fatal error:  The (unset) cast is no longer supported in %s on line 2\nStack trace:\n#0 {main}`,
    /// and for `-d xmlrpc_errors=1` the fault's string is
    /// `Fatal error:The (unset) cast … on line 2\nStack trace:\n#0 {main}`.
    #[test]
    fn the_log_form_and_the_xml_fault_carry_the_trace() {
        let file = b"witness.php";
        let engine = Engine::cli_builder()
            .ini("log_errors", "1")
            .ini("xmlrpc_errors", "1")
            .build();
        let mut req = request_on(&engine);
        req.note_error_in(Level::CompileError, "refused", file, 2);
        assert_eq!(
            req.take_aside(),
            vec![Aside::Log(
                b"PHP Fatal error:  refused in witness.php on line 2\nStack trace:\n#0 {main}"
                    .to_vec()
            )]
        );
        assert_eq!(
            String::from_utf8_lossy(&req.take_body()),
            "<?xml version=\"1.0\"?><methodResponse><fault><value><struct><member><name>faultCode</name><value><int>0</int></value></member><member><name>faultString</name><value><string>Fatal error:refused in witness.php on line 2\nStack trace:\n#0 {main}</string></value></member></struct></value></fault></methodResponse>"
        );
    }
}

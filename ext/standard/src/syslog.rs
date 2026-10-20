//! `Z/ext/standard/syslog.c`: `openlog`, `closelog` and `syslog` over the
//! host's `syslog(3)`, with `main/php_syslog.c`'s `php_syslog_str`
//! filter between, and the `LOG_*` constants with this host's values.
//! The ident `openlog(3)` keeps a pointer to lives on the request state,
//! `BG(syslog_device)`, and the log is closed at request shutdown, as
//! `PHP_RSHUTDOWN_FUNCTION(syslog)` closes it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::CString;

use php_ext::abi::True;
use php_ext::attr::{php_const, php_function};
use php_ext::request::Request;
use php_ext::values::PhpString;

use crate::State;

/// `BG(syslog_device)` and `PG(have_called_openlog)`.
#[derive(Default)]
pub(crate) struct Log {
    ident: Option<CString>,
    opened: bool,
}

/// The `LOG_*` constants of the stub, in its order.
///
/// ```php
/// var_dump(LOG_EMERG, LOG_DEBUG, LOG_KERN, LOG_USER, LOG_MAIL, LOG_AUTHPRIV, LOG_LOCAL0, LOG_LOCAL7, LOG_PID, LOG_CONS, LOG_ODELAY, LOG_NDELAY, LOG_NOWAIT, LOG_PERROR);
/// ```
/// ```expect
/// int(0)
/// int(7)
/// int(0)
/// int(8)
/// int(16)
/// int(80)
/// int(128)
/// int(184)
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(16)
/// int(32)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const LOG_EMERG: i64 = 0;
    pub(crate) const LOG_ALERT: i64 = 1;
    pub(crate) const LOG_CRIT: i64 = 2;
    pub(crate) const LOG_ERR: i64 = 3;
    pub(crate) const LOG_WARNING: i64 = 4;
    pub(crate) const LOG_NOTICE: i64 = 5;
    pub(crate) const LOG_INFO: i64 = 6;
    pub(crate) const LOG_DEBUG: i64 = 7;
    pub(crate) const LOG_KERN: i64 = 0;
    pub(crate) const LOG_USER: i64 = 8;
    pub(crate) const LOG_MAIL: i64 = 16;
    pub(crate) const LOG_DAEMON: i64 = 24;
    pub(crate) const LOG_AUTH: i64 = 32;
    pub(crate) const LOG_SYSLOG: i64 = 40;
    pub(crate) const LOG_LPR: i64 = 48;
    pub(crate) const LOG_NEWS: i64 = 56;
    pub(crate) const LOG_UUCP: i64 = 64;
    pub(crate) const LOG_CRON: i64 = 72;
    pub(crate) const LOG_AUTHPRIV: i64 = 80;
    pub(crate) const LOG_LOCAL0: i64 = 128;
    pub(crate) const LOG_LOCAL1: i64 = 136;
    pub(crate) const LOG_LOCAL2: i64 = 144;
    pub(crate) const LOG_LOCAL3: i64 = 152;
    pub(crate) const LOG_LOCAL4: i64 = 160;
    pub(crate) const LOG_LOCAL5: i64 = 168;
    pub(crate) const LOG_LOCAL6: i64 = 176;
    pub(crate) const LOG_LOCAL7: i64 = 184;
    pub(crate) const LOG_PID: i64 = 1;
    pub(crate) const LOG_CONS: i64 = 2;
    pub(crate) const LOG_ODELAY: i64 = 4;
    pub(crate) const LOG_NDELAY: i64 = 8;
    pub(crate) const LOG_NOWAIT: i64 = 16;
    pub(crate) const LOG_PERROR: i64 = 32;
}

/// The low 32 bits of a `zend_long`, as C's conversion to `int` keeps
/// them.
fn int(n: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let n = n as i32;
    n
}

/// `php_openlog`: `openlog(3)` with the ident the state keeps alive.
fn php_openlog(req: &mut Request, ident: CString, option: i32, facility: i32) {
    let log = &mut req.state::<State>().syslog;
    crate::ffi::openlog(&ident, option, facility);
    log.ident = Some(ident);
    log.opened = true;
}

/// `openlog(string $prefix, int $flags, int $facility): true`,
/// `PHP_FUNCTION(openlog)` (`Z/ext/standard/syslog.c:52`).
///
/// ```php
/// var_dump(openlog('php-witness', LOG_ODELAY, LOG_USER), closelog());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('openlog');
/// ```
/// ```expect
/// Function [ <internal:standard> function openlog ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $prefix ]
///     Parameter #1 [ <required> int $flags ]
///     Parameter #2 [ <required> int $facility ]
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn openlog(req: &mut Request, prefix: PhpString, flags: i64, facility: i64) -> True {
    let bytes = prefix.as_bytes();
    let ident = CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default();
    php_openlog(req, ident, int(flags), int(facility));
    True
}

/// `php_closelog`, and `BG(syslog_device)` given up; what
/// `PHP_RSHUTDOWN_FUNCTION(syslog)` does too.
pub(super) fn close(req: &mut Request) {
    crate::ffi::closelog();
    let log = &mut req.state::<State>().syslog;
    log.opened = false;
    log.ident = None;
}

/// `closelog(): true`, `PHP_FUNCTION(closelog)`
/// (`Z/ext/standard/syslog.c:73`).
///
/// ```php
/// var_dump(closelog());
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('closelog');
/// ```
/// ```expect
/// Function [ <internal:standard> function closelog ] {
///
///   - Parameters [0] {
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn closelog(req: &mut Request) -> True {
    close(req);
    True
}

/// `syslog.facility` as `OnSetFacility` (`Z/main/main.c:590`) reads its
/// name.
fn facility(req: &Request) -> i32 {
    let name = req.ini().get(b"syslog.facility").unwrap_or(b"LOG_USER");
    match name {
        b"LOG_AUTH" => 32,
        b"LOG_AUTHPRIV" => 80,
        b"LOG_CRON" => 72,
        b"LOG_DAEMON" => 24,
        b"LOG_FTP" => 88,
        b"LOG_KERN" => 0,
        b"LOG_LPR" => 48,
        b"LOG_MAIL" => 16,
        b"LOG_NEWS" => 56,
        b"LOG_SYSLOG" => 40,
        b"LOG_UUCP" => 64,
        b"LOG_LOCAL0" => 128,
        b"LOG_LOCAL1" => 136,
        b"LOG_LOCAL2" => 144,
        b"LOG_LOCAL3" => 152,
        b"LOG_LOCAL4" => 160,
        b"LOG_LOCAL5" => 168,
        b"LOG_LOCAL6" => 176,
        b"LOG_LOCAL7" => 184,
        _ => 8,
    }
}

/// `php_syslog` (`Z/main/php_syslog.c:57`): the log opened with
/// `syslog.ident` and `syslog.facility` where no `openlog` has been
/// called, then [`syslog_str`]. The engine's `request::Syslog`, so
/// `error_log=syslog` logs through it at its severity, on the
/// connection a userland `openlog()` opened; `LOG_PERROR` copies each
/// line to stderr, which a process of its own keeps apart.
///
/// ```php
/// openlog('php-witness-1364', LOG_PERROR, LOG_USER);
/// ini_set('log_errors', 1); ini_set('display_errors', 0); ini_set('error_log', 'syslog');
/// echo $undefined;
/// error_log("two\nlines\ttab");
/// trigger_error('gone', E_USER_DEPRECATED);
/// closelog();
/// ```
/// ```expect process
/// ```
/// ```stderr
/// %s php-witness-1364[%d] <Warning>: PHP Warning:  Undefined variable $undefined in %s on line 4
/// %s php-witness-1364[%d] <Notice>: two
/// %s php-witness-1364[%d] <Notice>: lines\x09tab
/// %s php-witness-1364[%d] <Info>: PHP Deprecated:  gone in %s on line 6
/// ```
pub(super) fn php_syslog(req: &mut Request, priority: i32, message: &[u8]) {
    if !req.state::<State>().syslog.opened {
        let ident = req.ini().get(b"syslog.ident").unwrap_or(b"php").to_vec();
        let ident = CString::new(ident).unwrap_or_default();
        let facility = facility(req);
        php_openlog(req, ident, 0, facility);
    }
    syslog_str(req, priority, message);
}

/// `php_syslog_str` (`Z/main/php_syslog.c:34`) under `syslog.filter`:
/// `raw` sends the message as it is; otherwise printable ASCII is kept,
/// bytes from `0x80` up where the filter is not `ascii`, control bytes
/// where it is `all`, a newline ends one message and begins the next,
/// and every other byte is `\xNN`.
fn syslog_str(req: &Request, priority: i32, message: &[u8]) {
    let filter = req.ini().get(b"syslog.filter").unwrap_or(b"no-ctrl");
    if filter == b"raw" {
        send(priority, message);
        return;
    }
    let mut line = Vec::new();
    for &c in message {
        match c {
            0x20..=0x7e => line.push(c),
            0x80..=0xff if filter != b"ascii" => line.push(c),
            b'\n' => {
                send(priority, &line);
                line.clear();
            }
            0x00..=0x1f if filter == b"all" => line.push(c),
            _ => line.extend_from_slice(format!("\\x{c:02x}").as_bytes()),
        }
    }
    send(priority, &line);
}

/// One `syslog(priority, "%s", ...)` of `php_syslog_str`: the line as
/// `%s` reads it, up to its first NUL, a NUL-terminated copy even when
/// it is empty. php hands `%s` a `zend_string`'s bytes under `raw` and
/// `%.*s` a `smart_string`'s otherwise; both end at a NUL, which only
/// `raw` and `all` let through.
fn send(priority: i32, line: &[u8]) {
    let line = php_ext::coerce::c_str(line);
    let line = CString::new(line).unwrap_or_default();
    crate::ffi::syslog(priority, &line);
}

/// `syslog(int $priority, string $message): true`,
/// `PHP_FUNCTION(syslog)` (`Z/ext/standard/syslog.c:88`): the message
/// through `php_syslog_str`.
///
/// ```php
/// var_dump(syslog(LOG_DEBUG, "php witness\tline"));
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// An empty message is one empty line, and a NUL ends the line as `%s`
/// reads it, under every filter. `LOG_INFO`, not `LOG_DEBUG`: the host's
/// default mask drops a debug line before it formats one.
///
/// ```php
/// var_dump(syslog(LOG_INFO, ""));
/// var_dump(syslog(LOG_INFO, "php witness\n"));
/// ini_set('syslog.filter', 'raw');
/// var_dump(syslog(LOG_INFO, ""));
/// var_dump(syslog(LOG_INFO, "php witness\0after"));
/// ini_set('syslog.filter', 'all');
/// var_dump(syslog(LOG_INFO, "php witness\0after"));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('syslog');
/// ```
/// ```expect
/// Function [ <internal:standard> function syslog ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $priority ]
///     Parameter #1 [ <required> string $message ]
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn syslog(req: &mut Request, priority: i64, message: PhpString) -> True {
    syslog_str(req, int(priority), message.as_bytes());
    True
}

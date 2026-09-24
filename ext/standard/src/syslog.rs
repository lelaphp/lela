//! `Z/ext/standard/syslog.c`: `openlog`, `closelog` and `syslog` over the
//! host's `syslog(3)`, with `main/php_syslog.c`'s `php_syslog_str`
//! filter between, and the `LOG_*` constants with this host's values.
//! The ident `openlog(3)` keeps a pointer to lives on the request state,
//! `BG(syslog_device)`, and the log is closed at request shutdown, as
//! `PHP_RSHUTDOWN_FUNCTION(syslog)` closes it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ffi::CString;

use php_ext::decl::{ConstDecl, FunctionDecl};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Value;

use crate::output::Entry;
use crate::stub::stub;
use crate::{State, argument_count_between, constant, long_argument, string_argument};

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
pub(crate) fn constants() -> Vec<ConstDecl> {
    [
        ("LOG_EMERG", 0),
        ("LOG_ALERT", 1),
        ("LOG_CRIT", 2),
        ("LOG_ERR", 3),
        ("LOG_WARNING", 4),
        ("LOG_NOTICE", 5),
        ("LOG_INFO", 6),
        ("LOG_DEBUG", 7),
        ("LOG_KERN", 0),
        ("LOG_USER", 8),
        ("LOG_MAIL", 16),
        ("LOG_DAEMON", 24),
        ("LOG_AUTH", 32),
        ("LOG_SYSLOG", 40),
        ("LOG_LPR", 48),
        ("LOG_NEWS", 56),
        ("LOG_UUCP", 64),
        ("LOG_CRON", 72),
        ("LOG_AUTHPRIV", 80),
        ("LOG_LOCAL0", 128),
        ("LOG_LOCAL1", 136),
        ("LOG_LOCAL2", 144),
        ("LOG_LOCAL3", 152),
        ("LOG_LOCAL4", 160),
        ("LOG_LOCAL5", 168),
        ("LOG_LOCAL6", 176),
        ("LOG_LOCAL7", 184),
        ("LOG_PID", 1),
        ("LOG_CONS", 2),
        ("LOG_ODELAY", 4),
        ("LOG_NDELAY", 8),
        ("LOG_NOWAIT", 16),
        ("LOG_PERROR", 32),
    ]
    .into_iter()
    .map(|(name, value)| constant(name.as_bytes(), value))
    .collect()
}

/// The three natives.
///
/// ```php
/// echo new ReflectionFunction('openlog'), new ReflectionFunction('closelog'), new ReflectionFunction('syslog');
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
/// Function [ <internal:standard> function closelog ] {
///
///   - Parameters [0] {
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function syslog ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $priority ]
///     Parameter #1 [ <required> string $message ]
///   }
///   - Return [ true ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub("openlog(string $prefix, int $flags, int $facility): true"),
            openlog as Entry,
        ),
        (stub("closelog(): true"), closelog),
        (stub("syslog(int $priority, string $message): true"), syslog),
    ]
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
fn openlog(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 3, 3)?;
    let ident = string_argument(req, 1)?;
    let option = long_argument(req, 2)?;
    let facility = long_argument(req, 3)?;
    let bytes = ident.as_bytes();
    let ident = CString::new(bytes.split(|c| *c == 0).next().unwrap_or(&[])).unwrap_or_default();
    php_openlog(req, ident, int(option), int(facility));
    Ok(Value::bool(true))
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
fn closelog(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    close(req);
    Ok(Value::bool(true))
}

/// `php_syslog_str` (`Z/main/php_syslog.c:34`) under `syslog.filter`:
/// `raw` sends the message as it is; otherwise printable ASCII is kept,
/// bytes from `0x80` up where the filter is not `ascii`, control bytes
/// where it is `all`, a newline ends one message and begins the next,
/// and every other byte is `\xNN`.
fn syslog_str(req: &Request, priority: i32, message: &[u8]) {
    let filter = req.ini_get(b"syslog.filter").unwrap_or(b"no-ctrl");
    if filter == b"raw" {
        crate::ffi::syslog(priority, message);
        return;
    }
    let mut line = Vec::new();
    for &c in message {
        match c {
            0x20..=0x7e => line.push(c),
            0x80..=0xff if filter != b"ascii" => line.push(c),
            b'\n' => {
                crate::ffi::syslog(priority, &line);
                line.clear();
            }
            0x00..=0x1f if filter == b"all" => line.push(c),
            _ => line.extend_from_slice(format!("\\x{c:02x}").as_bytes()),
        }
    }
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
fn syslog(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let priority = long_argument(req, 1)?;
    let message = string_argument(req, 2)?;
    syslog_str(req, int(priority), message.as_bytes());
    Ok(Value::bool(true))
}

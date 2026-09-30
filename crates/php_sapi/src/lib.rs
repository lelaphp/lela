//! The hosts' facade (ADR-0008; design §4.1, §4.3 seam 4, §5.14), as
//! `php_ext` is the extensions': one crate per host under `sapi/` builds a
//! request description from what it was given and supplies a sink, and
//! reaches the engine through this crate alone. What this crate imports
//! from `php_engine` is `sapi.lock`, the host face, re-exported here whole,
//! and `api` is never among it.
//!
//! Beside the face it holds only what php-src shares between its hosts in
//! `main/`: [`getopt`], `php_getopt`, which the CLI and CGI read their
//! command lines with; [`cgi`], the CGI environment made a request
//! description and the response's head written, which php's CGI and its
//! `FastCGI` process manager both do; [`ini`], the line's `-d` entries as
//! `php_ini_builder` reads them; and [`start`], `php_module_startup` over
//! them, with what a host prints before it serves anything.
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

pub mod cgi;
pub mod getopt;
pub mod ini;

use std::io::Write;
use std::process::ExitCode;

pub use php_engine::{
    CommandLine, Dispatched, EachLine, Engine, EngineBuilder, FileHandles, HostExtension, Method,
    Observables, OutputSink, PHP_VERSION, RequestDescription, ResponseHead, Routed, WebRequest,
};

/// `php_module_startup` for a host: `builder`, which carries the host's
/// extensions and tier, with the host's own extension after them where it
/// names one, the line's `-d` entries read by the engine's INI grammar
/// over its defaults in the order given, and what that reading wrote for
/// `stderr` written there, as php's `ini_error` writes it during startup.
#[must_use]
pub fn start(
    builder: EngineBuilder,
    ini: &[(String, String)],
    host: Option<HostExtension>,
) -> Engine {
    let mut builder = builder;
    if let Some(host) = host {
        builder = builder.host_extension(host);
    }
    #[cfg(feature = "witness-trace")]
    if let Some(path) = std::env::var_os(TRACE) {
        builder = builder.trace_natives_to(path.into());
    }
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    let engine = builder.build();
    let _ = std::io::stderr()
        .lock()
        .write_all(engine.startup_errors().as_bytes());
    engine
}

/// The variable naming the file a `witness-trace` build appends the
/// natives each request entered to; the witness lane sets it for a fence
/// it serves through a host's binary (harness §8, revision 61).
#[cfg(feature = "witness-trace")]
const TRACE: &str = "PHP_WITNESS_TRACE";

/// `print_modules` and `print_extensions`, which php's CLI and CGI each
/// write for `-m`: the loaded extensions sorted as `module_name_cmp` sorts
/// them, without regard to case, under `[PHP Modules]`, and the Zend
/// extensions in the order they were loaded under `[Zend Modules]`,
/// opcache's `Zend OPcache` being both.
#[must_use]
pub fn modules(engine: &Engine) -> String {
    let mut names = engine.loaded_extensions();
    names.sort_by_key(|name| name.to_ascii_lowercase());
    let mut text = String::from("[PHP Modules]\n");
    for name in names {
        text.push_str(&name);
        text.push('\n');
    }
    text.push_str("\n[Zend Modules]\n");
    for name in engine.loaded_zend_extensions() {
        text.push_str(&name);
        text.push('\n');
    }
    text.push('\n');
    text
}

/// The date a host's version line carries, php's `__DATE__ " " __TIME__`
/// of the compile that last rebuilt the host (`Z/main/main.c`), as `Jul
/// 28 2026 13:06:52`, the day padded with a space as `__DATE__` pads it:
/// rendered in UTC from `epoch`, the Unix seconds each host's `build.rs`
/// stamps when its sources change, which is when php's `__DATE__` moves.
/// The civil date is [`php_proto::http::civil`].
#[must_use]
pub fn build_date(epoch: &str) -> String {
    let at = php_proto::http::civil(epoch.parse().unwrap_or(0));
    let name = php_proto::http::MONTHS
        .get(usize::try_from(at.month).unwrap_or(1) - 1)
        .copied()
        .unwrap_or("Jan");
    format!(
        "{name} {:>2} {} {:02}:{:02}:{:02}",
        at.day, at.year, at.hour, at.minute, at.second
    )
}

/// `php_get_version` (`Z/main/main.c:128`), what each host's `-v`
/// prints: the head line with the host's name and the date `built`
/// carries, php's copyright line, and `get_zend_version`, the engine's
/// line and one for each Zend extension loaded; php starts its
/// extensions before it answers `-v`, so the engine is built first.
#[must_use]
pub fn version(name: &str, built: &str, engine: &Engine) -> String {
    format!(
        "PHP {PHP_VERSION} ({name}) (built: {}) (NTS)\nCopyright (c) The PHP Group\n{}",
        build_date(built),
        engine.zend_version_info()
    )
}

/// php's exit status as the process's: `exit()` takes the low byte.
#[must_use]
pub fn exit_code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status & 0xff).unwrap_or(255))
}

#[cfg(test)]
mod tests {
    use super::build_date;

    #[test]
    fn the_build_date_is_phps_date_and_time_in_utc() {
        assert_eq!(build_date("0"), "Jan  1 1970 00:00:00");
        assert_eq!(build_date("1785243412"), "Jul 28 2026 12:56:52");
        assert_eq!(build_date("951782400"), "Feb 29 2000 00:00:00");
    }
}

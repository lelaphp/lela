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
//! `php_ini_builder` reads them, and `php_init_config`'s search for the
//! php.ini files; and [`start`], `php_module_startup` over them, with what a
//! host prints before it serves anything. [`tier`] is
//! the tier face beside the host face, for the debugging host (#1308).
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

#[cfg(debug_assertions)]
use php_engine::LeakSensor;

pub use php_engine::{
    Aborted, Body, BuildDefs, CommandLine, Dispatched, EachLine, Engine, EngineBuilder,
    FileHandles, Forked, HostPage, HostServes, HostThread, HostValue, IniEvent, IniFile, IniFiles,
    IniOrigin, IniValue, Method, NotAlone, Observables, OutputSink, PHP_CONFIG_FILE_PATH,
    PHP_CONFIG_FILE_SCAN_DIR, PHP_LOCALSTATEDIR, PHP_PREFIX, PHP_SYSCONFDIR, PHP_VERSION, Pid,
    Primary, ProcessTitle, ProgramFile, RequestDescription, ResponseHead, Routed, Uncaught,
    WebRequest, fork, host_thread,
};

/// C's `strtol(s, NULL, 10)` and `atoi` as php's hosts call them on what
/// they were given, `fpm_conf.c`'s values, `php_cli.c`'s `--repeat` and
/// `PHP_CLI_SERVER_WORKERS`, phpdbg's numbers: the engine's one reading,
/// which an extension reads through `coerce` (#2634).
pub mod c_number {
    pub use php_engine::c_number::{atoi, strtol};
}

/// The tier face (`jit-design.md` §2.4, revision 8; ADR-0006): what a host
/// that binds a tier of its own, the debugging host, implements `Tier`
/// with, the stack it steps and what it writes between the steps,
/// re-exported as the host face is, so no host names `php_engine`. What
/// a frame, an op array or a linked table holds, the host reads through
/// the native face, as an extension does ([`tier::frame`]; #1911).
pub mod tier {
    pub use php_engine::tier::{
        Argument, Aside, Called, Evaluated, Flow, FrameRef, Hooked, Request, SafepointIdx, Site,
        Stepped, Tier, Under, begin, below, call, catches, entry, evaluate, evaluate_code, exit,
        frame, restore, set_argv, set_aside, set_stdin, step, unwind,
    };
}

/// `php_module_startup` for a host: `builder`, which carries the host's
/// name, its extensions and tier, and the extension and natives it
/// declared itself where it has any, the line's `-d` entries read by the
/// engine's INI grammar over its defaults in the order given, and what
/// that reading wrote for `stderr` written there, as php's `ini_error`
/// writes it during startup.
///
/// An extension whose start refused ends the process here, before the
/// host does anything of its own, as `php_error_cb` ends it with
/// `exit(-2)` for the `E_CORE_ERROR` of a failed `MINIT`
/// (`Z/main/main.c:1505-1507`) in every host alike: what php displays
/// written past the `php_output` layer to the process's stdout, or to its
/// stderr under `display_errors=stderr`, and what it logs handed to
/// `log_message`, the host's `sapi_module.log_message` as it stands
/// while php starts ([`Engine::refused`]).
///
/// Then the startup is finished here too, as `php_module_startup` runs
/// `zend_post_startup` before it returns ([`Engine::start_up`]): what the
/// extensions' starts raised displayed and logged once, and the
/// post-startup chain asked on the process's environment and working
/// directory, opcache's among it, with the `opcache.preload` request. A
/// refusal there, opcache's `opcache.file_cache_only` with no
/// `opcache.file_cache` or a preload that fails, ends the process with
/// php's status before the host opens its file, answers `-i` or binds a
/// socket.
///
/// The process's streams and `log_message` are left with the engine for
/// its end too ([`EngineBuilder::ends_into`]): what an extension's
/// engine-end function raises is displayed and logged there, as
/// `php_module_shutdown` writes it with `php_output` not activated.
#[must_use]
pub fn start(builder: EngineBuilder, ini: &[(Vec<u8>, Vec<u8>)], log_message: fn(&[u8])) -> Engine {
    let engine = prepare(builder, ini)
        .ends_into(Box::new(Starting(log_message)))
        .build();
    std::io::stderr()
        .lock()
        .write_all(engine.startup_errors())
        .ok();
    let mut starting = Starting(log_message);
    let env = std::env::vars_os()
        .map(|(n, v)| (n.into_encoded_bytes(), v.into_encoded_bytes()))
        .collect();
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(status) =
        (engine.refused(&mut starting)).or_else(|| engine.start_up(env, cwd, &mut starting))
    {
        std::io::stdout().lock().flush().ok();
        std::process::exit(status);
    }
    engine
}

/// Where php writes while it starts, before any request, and while it
/// ends, after the last: the body
/// straight to the process's stdout (`php_output_direct`, which
/// `php_output_startup` points at `php_output_stdout`), the error stream
/// to its stderr, no head, and the log through the host's function, each
/// as it is written.
struct Starting(fn(&[u8]));

impl OutputSink for Starting {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        std::io::stdout().lock().write_all(bytes).ok();
        Ok(())
    }

    fn flush(&mut self) {
        std::io::stdout().lock().flush().ok();
    }

    fn send_headers(&mut self, _head: &ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        (self.0)(entry);
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }

    /// Each call reaches the process as it is made, so a diagnostic's
    /// log line and its display stand in the order `php_error_cb` writes
    /// them.
    fn held(&mut self) -> Option<Box<dyn OutputSink + Send>> {
        Some(Box::new(Starting(self.0)))
    }
}

/// [`start`]'s builder before it is built: the witness lane's trace, a
/// debug build's request leak sensor, armed unless `PHP_SENSE_LEAKS` turns
/// it off and traced where `PHP_SENSE_LEAKS_BACKTRACE` asks for a
/// backtrace per allocation, and the line's `-d` entries.
fn prepare(builder: EngineBuilder, ini: &[(Vec<u8>, Vec<u8>)]) -> EngineBuilder {
    let mut builder = builder;
    if let Some(path) = std::env::var_os(TRACE) {
        builder = builder.trace_natives_to(path.into());
    }
    #[cfg(debug_assertions)]
    if let Some(sensor) = leak_sensor(|name| std::env::var_os(name)) {
        builder = builder.sense_leaks(sensor);
    }
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    builder
}

/// The variable naming the file every request appends the natives it
/// entered to; the witness lane sets it for a fence it serves through a
/// host's binary (harness §8, revision 61), and a host's own binary reads
/// it, with no build of its own for the lane (#2168).
const TRACE: &str = "PHP_WITNESS_TRACE";

/// The leak sensor the host's environment asks for in place of the
/// builder's armed default: traced where [`SENSE_LEAKS_BACKTRACE`] is `1`,
/// off where [`SENSE_LEAKS`] is `0`, and the default kept otherwise.
#[cfg(debug_assertions)]
fn leak_sensor(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<LeakSensor> {
    let is = |name, value: &str| var(name).is_some_and(|set| set == value);
    if is(SENSE_LEAKS_BACKTRACE, "1") {
        Some(LeakSensor::Traced)
    } else if is(SENSE_LEAKS, "0") {
        Some(LeakSensor::Off)
    } else {
        None
    }
}

/// The variable that turns a debug build's request leak sensor (#1992,
/// `EngineBuilder::sense_leaks`) off when it is `0`, so a debug request
/// ends as a release one does; the sensor is armed without it. Read once,
/// as the host builds its engine, and never per request. It is the
/// unarmed side of a census, the same debug binary measured unarmed to
/// tell a leak from a regression, `PHP_SENSE_LEAKS=0 target/debug/php`,
/// behind a wrapper given as `cargo xtask phpt --engine`, since the
/// recipe clears the environment; a release build reads nothing.
#[cfg(debug_assertions)]
const SENSE_LEAKS: &str = "PHP_SENSE_LEAKS";

/// The variable that arms a debug build's request leak sensor with a
/// backtrace kept per allocation (#1993, `LeakSensor::Traced`) when it is
/// `1`, whatever [`SENSE_LEAKS`] says: the leak then names the frames
/// that allocated its first block, each with its line in a build that
/// carries line tables, as the dev profile does, and by its function
/// alone in a `release-da` one. Read once, as [`SENSE_LEAKS`] is, and
/// for digging locally only, since every allocation pays a backtrace's
/// capture, which costs many times the allocation:
/// `PHP_SENSE_LEAKS_BACKTRACE=1 target/debug/php leaking.php`.
#[cfg(debug_assertions)]
const SENSE_LEAKS_BACKTRACE: &str = "PHP_SENSE_LEAKS_BACKTRACE";

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

/// `php_get_version` (`Z/main/main.c:128`), what each host's `-v`
/// prints: the head line with the host's name and the build's date, php's
/// copyright line, `Built by` and the provider where the build named one,
/// and `get_zend_version`, the engine's line and one for each Zend
/// extension loaded; php starts its extensions before it answers `-v`, so
/// the engine is built first. The date and the provider are the engine's
/// [`BuildDefs`], which `PHP_BUILD_DATE` and phpinfo read too.
#[must_use]
pub fn version(name: &str, engine: &Engine) -> String {
    let defs = engine.build_defs();
    let provider = defs
        .provider()
        .map(|provider| format!("Built by {provider}\n"))
        .unwrap_or_default();
    format!(
        "PHP {PHP_VERSION} ({name}) (built: {}) (NTS)\nCopyright (c) The PHP Group\n{provider}{}",
        defs.date(),
        engine.zend_version_info()
    )
}

/// php's exit status as the process's: `exit()` takes the low byte.
#[must_use]
pub fn exit_code(status: i32) -> ExitCode {
    ExitCode::from(u8::try_from(status & 0xff).unwrap_or(255))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use std::ffi::OsString;

    use php_engine::LeakSensor;

    use super::{SENSE_LEAKS, SENSE_LEAKS_BACKTRACE, leak_sensor};

    /// The sensor `leak_sensor` picks where the environment holds `set`.
    fn picked(set: &[(&str, &str)]) -> Option<LeakSensor> {
        leak_sensor(|name| {
            set.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    /// A debug host keeps the builder's armed sensor unless
    /// `PHP_SENSE_LEAKS=0` turns it off; `1`, the opt-in it once was,
    /// changes nothing, and `PHP_SENSE_LEAKS_BACKTRACE=1` traces whatever
    /// the other says (#1992).
    #[test]
    fn the_host_keeps_the_armed_default_unless_turned_off() {
        assert_eq!(picked(&[]), None);
        assert_eq!(picked(&[(SENSE_LEAKS, "1")]), None);
        assert_eq!(picked(&[(SENSE_LEAKS, "")]), None);
        assert_eq!(picked(&[(SENSE_LEAKS, "0")]), Some(LeakSensor::Off));
        assert_eq!(
            picked(&[(SENSE_LEAKS, "0"), (SENSE_LEAKS_BACKTRACE, "1")]),
            Some(LeakSensor::Traced)
        );
        assert_eq!(picked(&[(SENSE_LEAKS_BACKTRACE, "0")]), None);
    }
}

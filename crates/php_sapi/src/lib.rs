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
//! php.ini files; and [`start`], `php_module_startup` over
//! them, with what a host prints before it serves anything. [`tier`] is
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

pub use php_engine::{
    Aborted, Body, BuildDefs, CommandLine, Dispatched, EachLine, Engine, EngineBuilder,
    FileHandles, HostValue, IniEvent, IniFile, IniFiles, IniOrigin, IniValue, Method, Observables,
    OutputSink, PHP_INI_PATH, PHP_INI_SCAN_PATH, PHP_VERSION, Primary, ProcessTitle,
    RequestDescription, ResponseHead, Routed, Uncaught, WebRequest,
};

/// The tier face (`jit-design.md` §2.4, revision 8; ADR-0006): what a host
/// that binds a tier of its own, the debugging host, implements `Tier`
/// with and reads between the steps it drives, re-exported as the host
/// face is, so no host names `php_engine`.
pub mod tier {
    pub use php_engine::tier::{
        Argument, Aside, Called, Class, Code, Evaluated, Fatal, Flow, FrameRef, Identity, Key,
        Named, Opcode, Pending, Position, Request, SafepointIdx, Serves, Shape, Shown, Site,
        Stepped, Tier, TraceRow, Under, Variable, backtrace, below, call, caller, catches,
        class_named, entry, evaluate, evaluate_code, exit, exiting, frame_code, function_exists,
        function_named, included_files, last_error, literals, method_named, oplines, pending,
        position, restore, serves, set_argv, set_aside, set_stdin, step, superglobals, table_sizes,
        unwind, user_classes, user_constants, user_functions, variable, variables,
    };
}

/// `php_module_startup` for a host: `builder`, which carries the host's
/// name, its extensions and tier, and the extension and natives it
/// declared itself where it has any, the line's `-d` entries read by the
/// engine's INI grammar over its defaults in the order given, and what
/// that reading wrote for `stderr` written there, as php's `ini_error`
/// writes it during startup.
#[must_use]
pub fn start(builder: EngineBuilder, ini: &[(String, String)]) -> Engine {
    let engine = prepare(builder, ini).build();
    let _ = std::io::stderr()
        .lock()
        .write_all(engine.startup_errors().as_bytes());
    engine
}

/// [`start`]'s builder before it is built: the witness lane's trace and
/// the line's `-d` entries; for a host that builds more than one engine
/// and writes the startup errors once, as php-fpm's children build theirs
/// after the master has started.
#[must_use]
pub fn prepare(builder: EngineBuilder, ini: &[(String, String)]) -> EngineBuilder {
    let mut builder = builder;
    #[cfg(feature = "witness-trace")]
    if let Some(path) = std::env::var_os(TRACE) {
        builder = builder.trace_natives_to(path.into());
    }
    for (name, value) in ini {
        builder = builder.ini(name, value);
    }
    builder
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

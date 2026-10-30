//! php's CLI, `php-src/sapi/cli`, as one crate under `sapi/` (ADR-0008):
//! the binary `php`, reaching the engine through `php_sapi` alone. The
//! host is `sapi/cli/php_cli.c`: the command line read as `main` and
//! `do_cli` read it through `php_getopt` ([`options`]), one request
//! description built from it, the sink on the process's two streams, and
//! the exit code as `serve` ends the request ([`request`]); design §4.1,
//! §4.3 seam 4, §5.14.
//!
//! What the command line asks for is one [`Ask`], decoded once: the usage,
//! the version, `phpinfo()` as text, the extension listing, a request
//! over one program, `-r`'s code, stdin's lines under `-B`, `-R`, `-F`
//! and `-E`, `-l`'s lint of each program named, `-s` and `-w`, the
//! program highlighted or stripped as `zend_highlight` and `zend_strip`
//! write it, the reports `--rf`, `--rc`, `--re`, `--rz`, `--ri` and
//! `--ini` print, the interactive shell `-a` serves where the bundle
//! provides one, or the built-in web server. `-c`, `-n` and every `-d` are
//! the engine's to read before it is built, the php.ini found as
//! `php_init_config` finds it ([`php_sapi::ini::search`]) between the
//! CLI's `ini_defaults` and its `HARDCODED_INI` before the `-d`s, so the
//! binary reads them off [`Options`] and builds the engine; the host then serves
//! [`Options::ask`] on it, `--repeat`'s count of times.
//!
//! The binary carries php's shell callbacks
//! ([`php_sapi::EngineBuilder::cli_shell`]), which the readline extension
//! fills, so `-a` asks the engine whether it has a shell, as `do_cli`
//! asks `cli_shell_callbacks.cli_shell_run`, and is `do_cli`'s sentence
//! where it has none.
//!
//! `-e` builds the engine with extended info, as `main` sets
//! `ZEND_COMPILE_EXTENDED_INFO` once `php_module_startup` has returned
//! (`Z/sapi/cli/php_cli.c:1366`), for the built-in server's engine as for the CLI's
//! ([`php_sapi::EngineBuilder::extended_info`]). The switches taken and
//! changing nothing: `-H`, which blanks copies
//! of the arguments nothing reads again; `-C` and `-q`, php's own no-ops. `--ini=diff` is refused with
//! php's usage: the engine keeps no default apart from an entry's value.
//!
//! Where `php_getopt` and `do_cli` read stdin lazily, this host reads it
//! whole where it is the program: nothing in the engine reads the
//! request's body yet (design §4.3 seam 4, the body stream). Stdin's
//! lines under `-R` and `-F` are read through the request's `STDIN`, as
//! php reads them.

#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

#[allow(unsafe_code)]
mod ffi;
mod natives;
pub mod options;
pub mod request;
pub mod server;

use std::ffi::OsString;
use std::io::Write;

use std::cell::OnceCell;
use std::sync::OnceLock;

use php_sapi::{Engine, EngineBuilder, OutputSink};

use crate::natives::ProcessTitle;

/// What `php-linked --serve` does first, so a pool's server holds only
/// its own pipes (#2550).
#[cfg(feature = "linked")]
pub use ffi::close_inherited_fds;
pub use options::{
    Ask, Each, Lines, Listing, Options, Program, Refused, Server, Startup, usage_text,
};
pub use request::Process;

/// `sapi_module.name` for the CLI: what `php_sapi_name()` answers and
/// what the version line carries.
pub const NAME: &str = "cli";

/// The process's title, saved once first in [`main`] (`save_ps_args`),
/// which the process title's natives write and read for every request the
/// process serves.
static TITLE: OnceLock<Option<ProcessTitle>> = OnceLock::new();

/// The title, where [`main`] saved one.
pub(crate) fn process_title() -> Option<&'static ProcessTitle> {
    TITLE.get().and_then(Option::as_ref)
}

/// `PHP_CLI_PROCESS_TITLE` for a request of the CLI: whether [`main`]
/// saved the area.
pub(crate) fn title_constant() -> php_sapi::ProcessTitle {
    if process_title().is_some() {
        php_sapi::ProcessTitle::Saved
    } else {
        php_sapi::ProcessTitle::Unsaved
    }
}

/// `php_print_version` for this host, [`php_sapi::version`] under its
/// name.
#[must_use]
pub fn version(engine: &Engine) -> String {
    php_sapi::version(NAME, engine)
}

/// php's CLI `main`: the command line read, what it asks for served on
/// one engine of the bundle's enabled set with the bundle's tier bound,
/// and the process ending with the code the request ended with. The ini
/// the line carries is laid over the engine's defaults before it is
/// built, as php's `-d` reaches `php_ini_builder` before
/// `php_module_startup`. The engine is started once per process, as
/// `main` runs `php_module_startup` once: where `do_cli` reads the line,
/// [`Options::parse`] starts it and asks it whether it has a shell, and
/// the line is then served on it.
///
/// The process's title is saved here, before any thread starts; the rest
/// is served on the thread the process's work moves onto, of
/// [`php_sapi::REQUEST_STACK`] bytes, so recursion through a native's
/// callback reaches the depth php reaches on its main thread (design
/// §5.6, #3043), and the process ends there, in the child of a fork as
/// in the process that started it.
pub fn main() -> ! {
    // The host's own copy of its arguments, taken before `save_ps_args`:
    // on Linux the standard library reads them from the C runtime's
    // `argv`, whose `argv[1..]` the area's set-up points at its last NUL.
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `save_ps_args`, which php's `main` calls before anything else reads
    // `argv` (`php_cli.c:1224-1228`).
    TITLE.set(ffi::save_ps_args().map(ProcessTitle::new)).ok();
    php_sapi::main_on_request_stack(move || served(&argv))
}

/// What [`main`] serves, the command line read and served, and the
/// status it ends the process with, C's `stdout` flushed before the
/// engine ends, as `php_sapi::exit_code` flushed it when `main` returned.
fn served(argv: &[OsString]) -> i32 {
    let started = OnceCell::new();
    let cli = |startup: &Startup| started.get_or_init(|| engine(startup));
    let options = match Options::parse(argv, &|startup| cli(startup).has_shell()) {
        Ok(options) => options,
        Err(refused) => {
            std::io::stderr()
                .lock()
                .write_all(refused.stderr.as_bytes())
                .ok();
            std::io::stdout()
                .lock()
                .write_all(refused.stdout.as_bytes())
                .ok();
            php_sapi::stdio::flush();
            return refused.exit_code;
        }
    };
    let startup = &options.startup;
    let status = match &options.ask {
        Ask::Usage => print(&usage_text()),
        Ask::Version => print(&version(cli(startup))),
        Ask::Info => request::info(cli(startup)),
        Ask::Extensions => listed(cli(startup)),
        Ask::Server(server) => {
            let engine = server_engine(startup);
            let color = php_sapi::ini::enabled(&engine, "cli_server.color");
            server::serve(engine, server, color)
        }
        ask => {
            if options.interactive {
                print("Interactive shell\n\n");
            }
            repeated(cli(startup), ask, options.repeat)
        }
    };
    php_sapi::stdio::flush();
    status
}

/// `do_repeat` (`php_cli.c:854`, `:1170-1182`): `ask` served `repeat`
/// times, php's two sentences around the repeats, and the status the
/// last one left; a process a request forked does not repeat.
fn repeated(engine: &Engine, ask: &Ask, repeat: i64) -> i32 {
    let pid = std::process::id();
    let mut left = repeat.max(1);
    if left > 1 {
        print("Executing for the first time...\n");
    }
    loop {
        let status = once(engine, ask);
        left -= 1;
        if left == 0 || pid != std::process::id() {
            return status;
        }
        print("Finished execution, repeating...\n");
    }
}

/// `ask` served once.
fn once(engine: &Engine, ask: &Ask) -> i32 {
    match ask {
        Ask::Serve(program, arguments) => request::serve(engine, program, arguments),
        Ask::Lines(lines, arguments) => request::process_stdin(engine, lines, arguments),
        Ask::Highlight(program, arguments) => request::highlight(engine, program, arguments),
        Ask::Strip(program, arguments) => request::strip(engine, program, arguments),
        Ask::Listing(listing, file) => request::listing(engine, listing, file.as_deref()),
        Ask::Shell(file, arguments) => request::shell(engine, file.as_deref(), arguments),
        // `do_cli`'s lint loop: the status starts at 0, a lint that fails
        // sets 255 and a file that cannot be opened sets 1, each as it
        // comes, and one that passes changes nothing.
        Ask::Lint { programs, refused } => {
            let mut status = 0;
            if let Some(sentence) = refused {
                print(sentence);
                status = 1;
            }
            let mut sink: Box<dyn OutputSink + Send> = Box::new(Process);
            for program in programs {
                let (linted, back) = request::lint(engine, program, sink);
                sink = back;
                if linted != 0 {
                    status = linted;
                }
            }
            status
        }
        Ask::Usage | Ask::Version | Ask::Info | Ask::Extensions | Ask::Server(_) => 0,
    }
}

/// `-m` (`php_cli.c:643-655`), which prints in a request of its own
/// ([`request::started`]) once `php_module_startup` has returned, as `-v`
/// prints with none: what the startup raised displayed first either way,
/// as [`php_sapi::start`] finishes the startup, and where it failed
/// nothing printed and its status the answer.
///
/// ```php
/// foreach (['-v', '-m'] as $ask) {
///     foreach ([['-d', 'magic_quotes_gpc=1'], ['-d', 'extension_dir=/nope', '-d', 'extension=x']] as $d) {
///         $p = proc_open([PHP_BINARY, '-n', ...$d, $ask], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
///         $out = stream_get_contents($pipes[1]); echo stream_get_contents($pipes[2]), proc_close($p), "\n";
///         echo preg_replace(['/ \(tried: .*\)\)/', '/(PHP \d|\[PHP Modules\]).*/s'], ['', '$1'], $out), "\n";
///     }
/// }
/// ```
/// ```expect process
/// 1
///
/// Fatal error: Directive 'magic_quotes_gpc' is no longer available in PHP in Unknown on line 0
/// Stack trace:
/// #0 {main}
///
/// 0
///
/// Warning: PHP Startup: Unable to load dynamic library 'x' in Unknown on line 0
/// PHP 8
/// 1
///
/// Fatal error: Directive 'magic_quotes_gpc' is no longer available in PHP in Unknown on line 0
/// Stack trace:
/// #0 {main}
///
/// 0
///
/// Warning: PHP Startup: Unable to load dynamic library 'x' in Unknown on line 0
/// [PHP Modules]
/// ```
fn listed(engine: &Engine) -> i32 {
    match request::started(engine) {
        0 => print(&php_sapi::modules(engine)),
        failed => failed,
    }
}

/// `text` on stdout, and the status 0.
fn print(text: &str) -> i32 {
    std::io::stdout().lock().write_all(text.as_bytes()).ok();
    0
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, the CLI's `additional_functions`
/// under `standard` ([`natives::cli`]), the shebang line of every file
/// passed over as `do_cli` sets `CG(skip_shebang)`, php's shell callbacks
/// carried, started over the CLI's defaults, the php.ini files found
/// under the line's `-c` and `-n`, its `HARDCODED_INI` and the line's
/// `-d`s ([`php_sapi::start`]); with extended info where the line carried
/// `-e`.
///
/// A call of its own, so the builder, two kilobytes moved through each
/// step, stands in a frame that is gone before the first request is
/// served, and not under every frame of it (#3209).
#[inline(never)]
fn engine(startup: &Startup) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = cli_defaults(Engine::builder())
        .ini_files(php_sapi::ini::search(NAME, &startup.ini_search, true))
        .sapi_name(NAME)
        .extensions(extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .skip_shebang()
        .cli_shell()
        .extended_info(startup.extended_info);
    let builder = php_sapi::ini::CLI_HARDCODED
        .iter()
        .fold(builder, |builder, (name, value)| builder.ini(name, value));
    let engine = php_sapi::start(natives::cli(builder), &startup.ini, |entry| {
        request::Process.log(entry);
    });
    // `sapi_cli_deactivate`, which `sapi_deactivate` calls as
    // `php_module_startup` ends, flushes stdout: what php wrote while it
    // started is written before anything the CLI does.
    php_sapi::stdio::flush();
    engine
}

/// `sapi_cli_ini_defaults`, which the CLI and its built-in server both set.
fn cli_defaults(builder: EngineBuilder) -> EngineBuilder {
    php_sapi::ini::CLI_DEFAULTS
        .iter()
        .fold(builder, |builder, (name, value)| {
            builder.ini_default(name, value)
        })
}

/// The built-in server's engine: the bundle as [`engine`] builds it,
/// without the CLI's `HARDCODED_INI`, which `main` lays only under the
/// CLI's `sapi_module` (`php_cli.c:1336`), and `cli_server_module_entry` and its
/// natives after the bundle; with extended info where the line carried
/// `-e`.
///
/// [`php_sapi::start`] finishes its startup as `main` runs
/// `php_module_startup` before `-S` (`php_cli.c:1341`): what the startup
/// raised displayed to C's `stdout` under the server's name
/// ([`php_sapi::stdio`]), held there to the process's exit, since the
/// server's `sapi_module` has no `deactivate` to flush it as the CLI's does; no
/// extension's `rinit` called, and a startup that failed leaves no server.
/// The server's responses carry none of it, since php wrote it before any
/// client connected.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/srv' . bin2hex(random_bytes(8));
/// mkdir($dir);
/// file_put_contents("$dir/index.php", '<?php echo "body\n";');
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'extension_dir=/nope', '-d', 'extension=x', '-d', 'disable_functions=exit', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $ready = [$pipes[2]]; $none = null;
/// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
/// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
/// echo file_get_contents("http://127.0.0.1:$port/"), file_get_contents("http://127.0.0.1:$port/");
/// proc_terminate($p); proc_close($p);
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'magic_quotes_gpc=1', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// unlink("$dir/index.php"); rmdir($dir);
/// ```
/// ```expect process
/// body
/// body
/// <br />
/// <b>Fatal error</b>:  Directive 'magic_quotes_gpc' is no longer available in PHP in <b>Unknown</b> on line <b>0</b><br />
/// Stack trace:
/// #0 {main}
/// 1
/// ```
///
/// A call of its own, as [`engine`] is.
#[inline(never)]
fn server_engine(startup: &Startup) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = cli_defaults(Engine::builder())
        .ini_files(php_sapi::ini::search(
            server::NAME,
            &startup.ini_search,
            true,
        ))
        .sapi_name(server::NAME)
        .extensions(extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .skip_shebang()
        .extended_info(startup.extended_info);
    php_sapi::start(natives::server(builder), &startup.ini, server::log_message)
}

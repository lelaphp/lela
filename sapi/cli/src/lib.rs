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
//! `--ini` print, or the built-in web server. `-n` and every `-d` are the
//! engine's to lay over its defaults before it is built, so the binary
//! reads them off [`Options`] and builds the engine; the host then serves
//! [`Options::ask`] on it, `--repeat`'s count of times.
//!
//! The switches taken and changing nothing: `-a`, which is `do_cli`'s
//! sentence for a build without the readline extension, since this
//! engine has none; `-c`, `-e` and `-H`: no `php.ini` is looked for
//! anywhere, the compiler emits no extended info, and `-H` blanks copies
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
use std::process::ExitCode;

use std::sync::OnceLock;

use php_sapi::Engine;

use crate::natives::ProcessTitle;

pub use options::{Ask, Each, Lines, Options, Program, Refused, Report, Server, usage_text};
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
/// `php_module_startup`.
pub fn main() -> ExitCode {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `save_ps_args`, which php's `main` calls before anything else reads
    // `argv` (`php_cli.c:1224-1228`); the arguments were copied above.
    let _ = TITLE.set(ffi::save_ps_args().map(ProcessTitle::new));
    let options = match Options::parse(&argv) {
        Ok(options) => options,
        Err(refused) => {
            let _ = std::io::stderr()
                .lock()
                .write_all(refused.stderr.as_bytes());
            let _ = std::io::stdout()
                .lock()
                .write_all(refused.stdout.as_bytes());
            return php_sapi::exit_code(refused.exit_code);
        }
    };
    let status = match &options.ask {
        Ask::Usage => print(&usage_text()),
        Ask::Version => print(&version(&engine(&options.ini))),
        Ask::Info => request::info(&engine(&options.ini)),
        Ask::Extensions => print(&php_sapi::modules(&engine(&options.ini))),
        Ask::Server(server) => server::run(
            server_engine(&options.ini),
            server,
            php_sapi::ini::enabled(&options.ini, "cli_server.color", false),
        ),
        ask => repeated(&engine(&options.ini), ask, options.repeat),
    };
    php_sapi::exit_code(status)
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
        Ask::Report(report, file) => request::report(engine, report, file.as_deref()),
        // `do_cli`'s lint loop: the status starts at 0, a lint that fails
        // sets 255 and a file that cannot be opened sets 1, each as it
        // comes, and one that passes changes nothing.
        Ask::Lint { programs, refused } => {
            let mut status = 0;
            if let Some(sentence) = refused {
                print(sentence);
                status = 1;
            }
            let mut sink = Process;
            for program in programs {
                match request::lint(engine, program, &mut sink) {
                    0 => {}
                    ended => status = ended,
                }
            }
            status
        }
        Ask::Usage | Ask::Version | Ask::Info | Ask::Extensions | Ask::Server(_) => 0,
    }
}

/// `text` on stdout, and the status 0.
fn print(text: &str) -> i32 {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    0
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, the CLI's `additional_functions`
/// under `standard` ([`natives::cli`]), the shebang line of every file
/// passed over as `do_cli` sets `CG(skip_shebang)`, started over the
/// line's `ini` ([`php_sapi::start`]).
fn engine(ini: &[(String, String)]) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .sapi_name(NAME)
        .extensions(&extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .skip_shebang();
    php_sapi::start(natives::cli(builder), ini)
}

/// The built-in server's engine: the bundle as [`engine`] builds it, with
/// php's own defaults for the two entries the CLI's `HARDCODED_INI`
/// moves, which `main` lays only under the CLI's module
/// (`php_cli.c:1336`), and the `cli_server` module and its natives after
/// the bundle.
fn server_engine(ini: &[(String, String)]) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .sapi_name(server::NAME)
        .extensions(&extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .skip_shebang()
        .ini("html_errors", "1")
        .ini("implicit_flush", "0")
        .ini("max_execution_time", "30");
    php_sapi::start(natives::server(builder), ini)
}

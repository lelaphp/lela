//! php's CGI, `php-src/sapi/cgi`, as one crate under `sapi/` (ADR-0008):
//! the binary `php-cgi`, reaching the engine through `php_sapi` alone. The
//! host is `sapi/cgi/cgi_main.c` without its `FastCGI` half: the command
//! line read as `main` reads it through `php_getopt`, the request of the
//! web the server describes in the environment read as `init_request_info`
//! reads it ([`php_sapi::cgi`]), one request description built from both,
//! the body read from stdin up to `CONTENT_LENGTH`, and a sink that writes
//! the response's head as `sapi_cgi_send_headers` writes it before the
//! body (design §4.1, §4.3 seam 4, §5.14).
//!
//! php-cgi is a CGI program where the environment says a server started
//! it (`SERVER_SOFTWARE`, `SERVER_NAME`, `GATEWAY_INTERFACE` or
//! `REQUEST_METHOD`): then the file is the environment's
//! `SCRIPT_FILENAME` or `PATH_TRANSLATED`, the switches past `-c`, `-n`,
//! `-d`, `-s`, `-h` and `-T` are not read, and the request must carry
//! `REDIRECT_STATUS` while `cgi.force_redirect` is on. Started from a shell it
//! reads its switches as a command line: the file named, or stdin, the
//! words after the file its `$argv` and, joined by `arg_separator.input`,
//! its query string, and `-q` or `-f` writing no head.
//!
//! What php-cgi takes that this host refuses, its usage and exit 1: `-b`,
//! the `FastCGI` server, which is the `FastCGI` worker's to serve. `-T`
//! is taken and times nothing; `-c` and `-e` are taken and change nothing,
//! as the CLI host's are; `-i` serves `phpinfo()`.

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

mod natives;

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::process::ExitCode;

use php_proto::cgi::{Environment, RequestInfo};
use php_sapi::cgi::{Disk, path_of};
use php_sapi::getopt::{Getopt, Opt, Taken, getopt_line, opt};
use php_sapi::ini::{Search, define, enabled};
use php_sapi::{
    Aborted, Chdir, Engine, HostPage, HostServes, IniFiles, IniOrigin, Observables, OutputSink,
    RequestDescription, ResponseHead, WebRequest,
};

/// `sapi_module.name` for php's CGI: what `php_sapi_name()` answers.
pub const NAME: &str = "cgi-fcgi";

/// `OPTIONS`, `cgi_main.c:142-163`, row for row; the `-` row that ends
/// php's table is the slice's end.
const OPTIONS: &[Opt] = &[
    opt(b'a', 0, "interactive"),
    opt(b'b', 1, "bindpath"),
    opt(b'C', 0, "no-chdir"),
    opt(b'c', 1, "php-ini"),
    opt(b'd', 1, "define"),
    opt(b'e', 0, "profile-info"),
    opt(b'f', 1, "file"),
    opt(b'h', 0, "help"),
    opt(b'i', 0, "info"),
    opt(b'l', 0, "syntax-check"),
    opt(b'm', 0, "modules"),
    opt(b'n', 0, "no-php-ini"),
    opt(b'q', 0, "no-header"),
    opt(b's', 0, "syntax-highlight"),
    opt(b's', 0, "syntax-highlighting"),
    opt(b'w', 0, "strip"),
    opt(b'?', 0, "usage"),
    opt(b'v', 0, "version"),
    opt(b'T', 1, "timing"),
];

/// What one request is made of, `main`'s `behavior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// `PHP_MODE_STANDARD`: the program served.
    Standard,
    /// `-l`: compiled and not served.
    Lint,
    /// `-s`: written as `zend_highlight` writes it.
    Highlight,
    /// `-w`: written as `zend_strip` writes it.
    Strip,
    /// `-i`: `php_print_info`, which this host serves as `phpinfo()`.
    Info,
}

/// What the command line and the environment ask of php-cgi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// `php_cgi_usage` on stdout, with `php_getopt`'s line on stderr for
    /// a switch it does not know, and the exit code.
    Usage { stderr: String, exit_code: i32 },
    /// `-v`: `php_print_version`.
    Version,
    /// `-m`: the loaded extensions.
    Extensions,
    /// A request.
    Serve(Invocation),
}

/// What started php-cgi, which the environment says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A server: `SERVER_SOFTWARE`, `SERVER_NAME`, `GATEWAY_INTERFACE` or
    /// `REQUEST_METHOD` stands in the environment.
    Server,
    /// A shell, with none of them.
    Shell,
}

/// Whether the response's head is written, `SG(request_info).no_headers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    Written,
    /// `-q`, `-f`, `-l`: not written, and counted sent from the start.
    Quiet,
}

/// One request as the command line shaped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub behavior: Behavior,
    pub origin: Origin,
    /// `-f`'s file.
    pub file: Option<Vec<u8>>,
    /// The words after the switches: the file, where `-f` named none, and
    /// its arguments.
    pub words: Vec<Vec<u8>>,
    pub head: Head,
    /// `-C`: the working directory stays where it is.
    pub no_chdir: bool,
    /// `-a`: `Interactive mode enabled` said first.
    pub interactive: bool,
    /// `-e`, read from a shell alone: the engine compiles with extended
    /// info (`Z/sapi/cgi/cgi_main.c:2307`).
    pub extended_info: bool,
}

/// The command line, read: the engine's entries and what is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Each `-d name=value`, in order.
    pub ini: Vec<(Vec<u8>, Vec<u8>)>,
    /// `-c` and `-n`, which steer the search for php.ini.
    pub ini_search: Search,
    pub ask: Ask,
}

impl Options {
    /// Read `argv[1..]` in `env` as `main` reads them
    /// (`Z/sapi/cgi/cgi_main.c:1787-2270`): whether this is CGI, whether a
    /// query string of a lone switch forbids reading the line at all, the
    /// first pass for the ini, the second for the usage, and, started from a
    /// shell, the third for the rest.
    #[must_use]
    pub fn parse(argv: &[OsString], env: &Environment) -> Options {
        let argv: Vec<Vec<u8>> = argv
            .iter()
            .map(|word| word.as_encoded_bytes().to_vec())
            .collect();
        let server = [
            &b"SERVER_SOFTWARE"[..],
            b"SERVER_NAME",
            b"GATEWAY_INTERFACE",
            b"REQUEST_METHOD",
        ]
        .iter()
        .any(|name| env.get(name).is_some());
        let skip = env
            .get(b"QUERY_STRING")
            .filter(|query| !query.contains(&b'='))
            .is_some_and(|query| decode(query).iter().find(|b| **b > b' ') == Some(&b'-'));
        let mut ini = Vec::new();
        let mut ini_search = Search::default();
        let mut invocation = Invocation {
            behavior: Behavior::Standard,
            origin: if server {
                Origin::Server
            } else {
                Origin::Shell
            },
            file: None,
            words: Vec::new(),
            head: Head::Written,
            no_chdir: false,
            interactive: false,
            extended_info: false,
        };
        if skip {
            return Options {
                ini,
                ini_search,
                ask: Ask::Serve(invocation),
            };
        }
        let mut refused = false;
        for (code, value) in switches(&argv) {
            match code {
                b'd' => ini.push(define(&value)),
                b'c' => ini_search.path = Some(value),
                b'n' => ini_search.ignore = true,
                b'b' => refused = true,
                b's' => invocation.behavior = Behavior::Highlight,
                _ => {}
            }
        }
        let ask = match usage_pass(&argv) {
            Some(usage) => usage,
            None if refused => Ask::Usage {
                stderr: String::new(),
                exit_code: 1,
            },
            None if server => Ask::Serve(invocation),
            None => shell_pass(&argv, invocation),
        };
        Options {
            ini,
            ini_search,
            ask,
        }
    }
}

/// `main`'s second pass, `show_err` on: the usage, and exit 0, for `-h`
/// and `-?`; `php_getopt`'s line and the usage, and exit 1, for a switch
/// it does not know.
fn usage_pass(argv: &[Vec<u8>]) -> Option<Ask> {
    let mut getopt = Getopt::new(OPTIONS, 0);
    loop {
        match getopt.next(argv) {
            Ok(Taken::End) => return None,
            Ok(Taken::Switch {
                code: b'h' | b'?', ..
            }) => {
                return Some(Ask::Usage {
                    stderr: String::new(),
                    exit_code: 0,
                });
            }
            Ok(Taken::Switch { .. }) => {}
            Err((argument, at, error)) => {
                return Some(Ask::Usage {
                    stderr: getopt_line(argv, argument, at, error),
                    exit_code: 1,
                });
            }
        }
    }
}

/// `main`'s third pass, from a shell alone: the switches that shape
/// the request, `-m` and `-v` answered in its place, and the words left.
fn shell_pass(argv: &[Vec<u8>], mut invocation: Invocation) -> Ask {
    let mut getopt = Getopt::new(OPTIONS, 0);
    while let Ok(Taken::Switch { code, value, .. }) = getopt.next(argv) {
        match code {
            b'a' => invocation.interactive = true,
            b'C' => invocation.no_chdir = true,
            b'e' => invocation.extended_info = true,
            b'f' => {
                invocation.file = value;
                invocation.head = Head::Quiet;
            }
            b'i' => invocation.behavior = Behavior::Info,
            b'l' => {
                invocation.head = Head::Quiet;
                invocation.behavior = Behavior::Lint;
            }
            b'm' => return Ask::Extensions,
            b'q' => invocation.head = Head::Quiet,
            b'v' => return Ask::Version,
            b'w' => invocation.behavior = Behavior::Strip,
            _ => {}
        }
        if invocation.behavior == Behavior::Info {
            break;
        }
    }
    invocation.words = argv[getopt.optind.min(argv.len())..].to_vec();
    Ask::Serve(invocation)
}

/// Every switch of one `php_getopt` pass that reads all it can, a switch
/// php does not know passed over as `show_err` 0 passes it.
fn switches(argv: &[Vec<u8>]) -> Vec<(u8, Vec<u8>)> {
    let mut getopt = Getopt::new(OPTIONS, 0);
    let mut taken = Vec::new();
    loop {
        match getopt.next(argv) {
            Ok(Taken::End) => return taken,
            Ok(Taken::Switch { code, value, .. }) => taken.push((code, value.unwrap_or_default())),
            Err(_) => {}
        }
    }
}

/// `php_url_decode`, for the query string `main` looks at before the line.
fn decode(query: &[u8]) -> Vec<u8> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut out = Vec::with_capacity(query.len());
    let mut i = 0;
    while i < query.len() {
        match query[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < query.len() => match (hex(query[i + 1]), hex(query[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// `php_cgi_usage` (`Z/sapi/cgi/cgi_main.c:1015`), the program named by
/// what follows the last `/` of `argv[0]`, or `php-cgi`.
#[must_use]
pub fn usage_text(argv0: &OsStr) -> String {
    let bytes = argv0.as_encoded_bytes();
    let prog = match bytes.iter().rposition(|b| *b == b'/') {
        Some(at) => String::from_utf8_lossy(&bytes[at + 1..]).into_owned(),
        None => String::from("php-cgi"),
    };
    format!(
        "Usage: {prog} [-q] [-h] [-s] [-v] [-i] [-f <file>]\n\
         \x20      {prog} <file> [args...]\n\
         \x20 -a               Run interactively\n\
         \x20 -b <address:port>|<port> Bind Path for external FASTCGI Server mode\n\
         \x20 -C               Do not chdir to the script's directory\n\
         \x20 -c <path>|<file> Look for php.ini file in this directory\n\
         \x20 -n               No php.ini file will be used\n\
         \x20 -d foo[=bar]     Define INI entry foo with value 'bar'\n\
         \x20 -e               Generate extended information for debugger/profiler\n\
         \x20 -f <file>        Parse <file>.  Implies `-q'\n\
         \x20 -h               This help\n\
         \x20 -i               PHP information\n\
         \x20 -l               Syntax check only (lint)\n\
         \x20 -m               Show compiled in modules\n\
         \x20 -q               Quiet-mode.  Suppress HTTP Header output.\n\
         \x20 -s               Display colour syntax highlighted source.\n\
         \x20 -v               Version number\n\
         \x20 -w               Display source with stripped comments and whitespace.\n\
         \x20 -T <count>       Measure execution time of script repeated <count> times.\n"
    )
}

/// `php_print_version` for this host, [`php_sapi::version`] under its
/// name.
#[must_use]
pub fn version(engine: &Engine) -> String {
    php_sapi::version(NAME, engine)
}

/// The process's standard streams as php-cgi writes them: the body on
/// stdout after the head `sapi_cgi_send_headers` writes, the log and the
/// error stream on stderr, each log entry with `\n` after it
/// (`sapi_cgi_log_message`).
#[derive(Debug, Default)]
pub struct Process;

impl OutputSink for Process {
    /// `sapi_cgi_ub_write`: a write stdout refuses is
    /// `php_handle_aborted_connection`, with no status set.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(bytes)
            .and_then(|()| stdout.flush())
            .map_err(|_| Aborted { exit_status: None })
    }

    fn flush(&mut self) {
        std::io::stdout().lock().flush().ok();
    }

    /// `sapi_cgi_send_headers`, the head [`php_sapi::cgi::head`] writes.
    ///
    /// `cgi.nph` read at the send, so an `ini_set` before the first byte
    /// opens the head with `Status: 200 OK`:
    ///
    /// ```php
    /// ini_set('cgi.nph', '1');
    /// echo "x";
    /// ```
    /// ```request
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// Status: 200 OK
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// x
    /// ```
    ///
    /// and `cgi.rfc2616_headers` writes the status line `header()` was
    /// given whole:
    ///
    /// ```php
    /// ini_set('cgi.rfc2616_headers', '1');
    /// header('HTTP/1.1 404 Not Found');
    /// echo "x";
    /// ```
    /// ```request
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// HTTP/1.1 404 Not Found
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// x
    /// ```
    fn send_headers(&mut self, head: &ResponseHead) {
        self.write(&php_sapi::cgi::head(head)).ok();
    }

    fn log(&mut self, entry: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(entry).ok();
        stderr.write_all(b"\n").ok();
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        std::io::stderr().lock().write_all(bytes).ok();
    }
}

/// What `main` refuses a request with before it starts, as it writes it.
enum Refusal {
    /// `cgi.force_redirect` with no `REDIRECT_STATUS`: the alert on stdout
    /// and no head.
    Redirect,
}

/// The security alert `main` writes where `cgi.force_redirect` finds no
/// redirect status (`Z/sapi/cgi/cgi_main.c:1886`).
const SECURITY_ALERT: &str = "<b>Security Alert!</b> The PHP CGI cannot be accessed directly.\n\n\
<p>This PHP CGI binary was compiled with force-cgi-redirect enabled.  This\n\
means that a page will only be served up if the REDIRECT_STATUS CGI variable is\n\
set, e.g. via an Apache Action directive.</p>\n\
<p>For more information as to <i>why</i> this behaviour exists, see the <a href=\"https://www.php.net/security.cgi-bin\">\
manual page for CGI security</a>.</p>\n\
<p>For more information about changing this behaviour or re-enabling this webserver,\n\
consult the installation file that came with this distribution, or visit \n\
<a href=\"https://www.php.net/install.windows\">the manual page</a>.</p>\n";

/// The request description for `invocation` in `env`, under the
/// `cgi.*`, `doc_root` and `arg_separator.input` entries `engine` started
/// with, php.ini's included, as `main` and `init_request_info` build
/// `SG(request_info)`: the file the environment names, or, from a shell,
/// the one `-f` or the first word names, or stdin; `$argv` from a shell
/// alone; the query string, the cookie header, the content type and its
/// length, and the body read from stdin up to that length; `PHP_SELF`
/// from `SCRIPT_NAME` and `PATH_INFO`, and the `Basic` credentials; the
/// working directory the file's own unless `-C`. The file is opened once
/// the request has started ([`serve`]), as `main` opens it after
/// `php_request_startup`. Where the request has a path translated,
/// `sapi_cgi_activate` sets the php.ini's `[HOST=]` table of its
/// `SERVER_NAME` and its `[PATH=]` tables along the path's directory
/// ([`Engine::activated_ini`]), then each `.user.ini` from
/// `DOCUMENT_ROOT` down to it ([`php_sapi::cgi::user_ini`]).
///
/// `cgi.fix_pathinfo` off in the engine's ini, as a php.ini sets it, is
/// php's behaviour before 4.3 (`Z/sapi/cgi/cgi_main.c:1426`): the request
/// URI is `PATH_INFO` where the server sets one, and `PHP_SELF` that URI
/// alone, where with the entry on it is `SCRIPT_NAME` and `PATH_INFO`
/// together (`/witness.php/extra`):
///
/// ```php
/// echo ini_get('cgi.fix_pathinfo'), "\n", $_SERVER['PHP_SELF'], "\n";
/// ```
/// ```request
/// --GET--
/// q=1
/// --ENV--
/// SCRIPT_NAME=/witness.php
/// PATH_INFO=/extra
/// ```
/// ```expect regime=cgi-fix-pathinfo-off
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// 0
/// /extra
/// ```
fn describe(
    engine: &Engine,
    invocation: &Invocation,
    mut env: Environment,
) -> Result<(RequestDescription, Primary), Refusal> {
    let server = invocation.origin == Origin::Server;
    let settings = php_sapi::cgi::settings(engine);
    if server && enabled(engine, "cgi.force_redirect") {
        // `NULL` falls back; an explicitly empty name finds nothing.
        let name = engine
            .ini_value(b"cgi.redirect_status_env")
            .unwrap_or_else(|| b"REDIRECT_STATUS".to_vec());
        if env.get(&name).is_none() {
            return Err(Refusal::Redirect);
        }
    }
    let doc_root = engine.ini_get(b"doc_root").filter(|root| !root.is_empty());
    let info: RequestInfo =
        php_proto::cgi::request_info(&mut env, settings, doc_root.as_deref(), &Disk);
    let (path, argv, query) = if server {
        (info.path_translated.clone(), Vec::new(), info.query.clone())
    } else {
        from_shell(engine, invocation, &info)
    };
    let mut activated = path.as_deref().map_or_else(Vec::new, |path| {
        engine.activated_ini(env.get(b"SERVER_NAME"), path)
    });
    if let (Some(path), Some(doc_root)) = (path.as_deref(), env.get(b"DOCUMENT_ROOT")) {
        let dir = php_sapi::cgi::user_ini_dir(path);
        let user = php_sapi::cgi::user_ini(engine, &dir, doc_root);
        activated.extend(user.into_iter().map(|(k, v)| (k, v, IniOrigin::PerDir)));
    }
    let primary = Primary::of(path, invocation);
    // The request starts in the process's directory, and
    // `php_execute_script` enters the program's but under `-C`,
    // `SAPI_OPTION_NO_CHDIR` (`Z/sapi/cgi/cgi_main.c:2304`); `-i` and
    // stdin serve no file to enter by.
    let chdir = match (&primary, invocation.no_chdir) {
        (Primary::Path(Some(_)), false) => Chdir::IntoProgramDir,
        _ => Chdir::Stay,
    };
    let rows = php_sapi::cgi::server_variables(&env, &info, settings);
    // The body, `sapi_cgi_read_post`'s stdin up to `CONTENT_LENGTH`, where
    // stdin is not the program.
    let mut body = Vec::new();
    if info.content_length > 0 && !matches!(primary, Primary::Stdin) {
        let length = u64::try_from(info.content_length).unwrap_or(0);
        std::io::stdin()
            .lock()
            .take(length)
            .read_to_end(&mut body)
            .ok();
    }
    let web = WebRequest {
        query,
        body: php_sapi::Body::from(body),
        no_headers: invocation.head == Head::Quiet,
        response_code: info.response_code,
        ..php_sapi::cgi::web(&env, &info)
    };
    let (script_name, source) = match &primary {
        Primary::Info(path) => (path.clone(), Some(b"<?php phpinfo();".to_vec())),
        Primary::Path(path) => (path.clone(), None),
        Primary::Stdin => {
            let mut source = Vec::new();
            std::io::stdin().lock().read_to_end(&mut source).ok();
            (Some(b"Standard input code".to_vec()), Some(source))
        }
    };
    Ok((
        RequestDescription {
            argv,
            env: env.0,
            cwd: std::env::current_dir().unwrap_or_default(),
            chdir,
            server: rows,
            sapi_name: String::from(NAME),
            web: Some(web),
            script_name,
            source,
            ini: activated,
            ..RequestDescription::default()
        },
        primary,
    ))
}

/// The file a request serves, its `$argv` and its query string.
type Served = (Option<Vec<u8>>, Vec<Vec<u8>>, Option<Vec<u8>>);

/// What a request started from a shell serves, `main` past its third pass
/// (`Z/sapi/cgi/cgi_main.c:2342-2394`): `-f`'s file with the words after
/// it its `$argv`, or the first word with the rest; and, where the
/// environment gives no query string, every word joined by the first
/// byte of `arg_separator.input`, the file among them.
fn from_shell(engine: &Engine, invocation: &Invocation, info: &RequestInfo) -> Served {
    let mut path = info.path_translated.clone();
    let mut argv = Vec::new();
    if let Some(script) = &invocation.file {
        path = Some(script.clone());
        argv.push(script.clone());
        argv.extend(invocation.words.iter().cloned());
    } else if let Some(first) = invocation.words.first() {
        path = Some(first.clone());
        argv.clone_from(&invocation.words);
    }
    let mut query = info.query.clone();
    if query.is_none() && !invocation.words.is_empty() {
        let separator = engine.ini_get(b"arg_separator.input").unwrap_or_default();
        let separator = separator.first().copied().unwrap_or(b'&');
        query = Some(invocation.words.join(&separator));
    }
    (path, argv, query)
}

/// What a request serves, as `main` settles it.
enum Primary {
    /// `-i`: `php_print_info`, no file opened, whatever the line named.
    Info(Option<Vec<u8>>),
    /// The file `php_fopen_primary_script` opens once the request has
    /// started; a server's request with no path translated has none.
    Path(Option<Vec<u8>>),
    /// Stdin, read whole as the request's source, where a shell named
    /// no file.
    Stdin,
}

impl Primary {
    /// The primary of `invocation` with `path` translated: a server never
    /// takes stdin, and a shell that named no file reads it.
    fn of(path: Option<Vec<u8>>, invocation: &Invocation) -> Primary {
        if invocation.behavior == Behavior::Info {
            return Primary::Info(path);
        }
        match (path, invocation.origin) {
            (None, Origin::Shell) => Primary::Stdin,
            (path, _) => Primary::Path(path),
        }
    }
}

/// `php_fopen_primary_script` failing over `path`, as `main` answers it
/// once the request has started (`Z/sapi/cgi/cgi_main.c:2478-2487`):
/// `Access denied.` under a 403 where the open met `EACCES`, and `No
/// input file specified.` under a 404 otherwise, a request with no path
/// among them; `None` where the file opens.
fn refusal(path: Option<&[u8]>) -> Option<HostPage> {
    let denied = match path.map(|path| std::fs::File::open(path_of(path))) {
        Some(Ok(_)) => return None,
        Some(Err(error)) => error.kind() == std::io::ErrorKind::PermissionDenied,
        None => false,
    };
    let (code, body): (i64, &[u8]) = if denied {
        (403, b"Access denied.\n")
    } else {
        (404, b"No input file specified.\n")
    };
    Some(HostPage {
        code,
        headers: Vec::new(),
        body: body.to_vec(),
    })
}

/// The process's environment, `environ`.
#[must_use]
pub fn environment() -> Environment {
    Environment(
        std::env::vars_os()
            .map(|(n, v)| (n.as_encoded_bytes().to_vec(), v.as_encoded_bytes().to_vec()))
            .collect(),
    )
}

/// Serve `invocation` on `engine` in `env` as php-cgi's `main` serves it,
/// and hand back the exit code.
///
/// A request with a query string, a cookie header and a form body, served
/// as upstream's test runner serves a CGI-shaped test: `$_GET`, `$_POST` and
/// `$_COOKIE` registered from them, and the head php-cgi writes before
/// the body, `X-Powered-By`, the program's own line and the default
/// `Content-type`:
///
/// ```php
/// header("X-Witness: 1");
/// var_dump($_GET, $_POST, $_COOKIE);
/// ```
/// ```request
/// --GET--
/// q=1&a[]=2
/// --COOKIE--
/// c=v; d=w
/// --POST--
/// p=Hello+World
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// X-Witness: 1
/// Content-type: text/html; charset=UTF-8
///
/// array(2) {
///   ["q"]=>
///   string(1) "1"
///   ["a"]=>
///   array(1) {
///     [0]=>
///     string(1) "2"
///   }
/// }
/// array(1) {
///   ["p"]=>
///   string(11) "Hello World"
/// }
/// array(2) {
///   ["c"]=>
///   string(1) "v"
///   ["d"]=>
///   string(1) "w"
/// }
/// ```
pub fn serve(engine: &Engine, invocation: &Invocation, env: Environment) -> i32 {
    if invocation.interactive {
        Process.write(b"Interactive mode enabled\n\n").ok();
    }
    if invocation.behavior == Behavior::Lint {
        return lint(engine, invocation, &env);
    }
    let (description, primary) = match describe(engine, invocation, env) {
        Ok(described) => described,
        Err(Refusal::Redirect) => {
            Process.write(SECURITY_ALERT.as_bytes()).ok();
            return 255;
        }
    };
    let sink = Box::new(Process);
    let mut refused = false;
    let (served, _): (Observables, _) = match (&primary, invocation.behavior) {
        (Primary::Path(path), Behavior::Standard) => engine.serve(description, sink, || {
            refusal(path.as_deref()).map_or(HostServes::Program, |page| {
                refused = true;
                HostServes::Page(page)
            })
        }),
        (Primary::Path(path), _) if let Some(page) = refusal(path.as_deref()) => {
            refused = true;
            engine.serve(description, sink, || HostServes::Page(page))
        }
        (_, Behavior::Highlight) => engine.highlight(description, sink),
        (_, Behavior::Strip) => engine.strip(description, sink),
        _ => engine.serve(description, sink, || HostServes::Program),
    };
    // `main` returns `FAILURE` where `php_request_startup` failed, an
    // extension's `rinit` having raised (#1775), and php-cgi ends there
    // (`Z/sapi/cgi/cgi_main.c:2460-2467`), or where the file did not open.
    if refused || served.start_failed {
        255
    } else {
        served.exit_code
    }
}

/// `-l`: each file compiled as its own request and its verdict written,
/// and while words remain after the one checked, the next checked
/// (`goto do_repeat`, `Z/sapi/cgi/cgi_main.c:2606`); a file that does not
/// open ends the process with 255 where it stands, and one that does not
/// compile leaves the exit 255 once the rest are checked.
fn lint(engine: &Engine, invocation: &Invocation, env: &Environment) -> i32 {
    let mut this = invocation.clone();
    let mut exit_code = 0;
    loop {
        let (description, primary) = match describe(engine, &this, env.clone()) {
            Ok(described) => described,
            Err(Refusal::Redirect) => {
                Process.write(SECURITY_ALERT.as_bytes()).ok();
                return 255;
            }
        };
        if let Primary::Path(path) = &primary
            && let Some(page) = refusal(path.as_deref())
        {
            engine.serve(description, Box::new(Process), || HostServes::Page(page));
            return 255;
        }
        let name = description.script_name.clone().unwrap_or_default();
        let (served, _) = engine.lint(description, Box::new(Process));
        // Each file's request is started at `do_repeat`, and one whose
        // start fails ends the process with `FAILURE`, no verdict written.
        if served.start_failed {
            return 255;
        }
        let mut line = if served.exit_code == 0 {
            b"No syntax errors detected in ".to_vec()
        } else {
            exit_code = 255;
            b"Errors parsing ".to_vec()
        };
        line.extend_from_slice(&name);
        line.push(b'\n');
        Process.write(&line).ok();
        if this.words.len() < 2 {
            return exit_code;
        }
        this.words.remove(0);
        this.file = None;
    }
}

/// php-cgi's `main`: the line read in the process's environment, and the
/// request served, or what the line asked for instead, on one engine of
/// the bundle's enabled set with `cgi-fcgi` loaded after it; the process
/// ends with the code the request ended with.
pub fn main() -> ExitCode {
    let mut words = std::env::args_os();
    let argv0 = words.next().unwrap_or_default();
    let argv: Vec<OsString> = words.collect();
    let env = environment();
    let options = Options::parse(&argv, &env);
    let engine = |ini: &[(Vec<u8>, Vec<u8>)], origin, extended_info| {
        engine(
            ini,
            php_sapi::ini::search(NAME, &options.ini_search, false),
            origin,
            extended_info,
        )
    };
    let status = match &options.ask {
        // `main` starts php before its second pass answers the usage
        // (`Z/sapi/cgi/cgi_main.c:1879`, `:2260-2269`).
        Ask::Usage { stderr, exit_code } => {
            drop(engine(&options.ini, Origin::Shell, false));
            std::io::stderr().lock().write_all(stderr.as_bytes()).ok();
            std::io::stdout()
                .lock()
                .write_all(usage_text(&argv0).as_bytes())
                .ok();
            *exit_code
        }
        // `-v` and `-m` are read by the pass a shell's line alone is given.
        Ask::Version => print(&version(&engine(&options.ini, Origin::Shell, false))),
        Ask::Extensions => print(&php_sapi::modules(&engine(
            &options.ini,
            Origin::Shell,
            false,
        ))),
        Ask::Serve(invocation) => serve(
            &engine(&options.ini, invocation.origin, invocation.extended_info),
            invocation,
            env,
        ),
    };
    php_sapi::exit_code(status)
}

/// `text` on stdout, and the status 0.
fn print(text: &str) -> i32 {
    std::io::stdout().lock().write_all(text.as_bytes()).ok();
    0
}

/// A fresh engine of `php_bundle::enabled()`, every extension in startup
/// order and `php_bundle::Tier` bound, and what php-cgi declares itself
/// after them ([`natives::declare`]), started over the `files` found and
/// the line's `ini` ([`php_sapi::start`]); a file's shebang
/// line passed over while `cgi.check_shebang_line` is on, as `main` sets
/// `CG(skip_shebang)` under it (`Z/sapi/cgi/cgi_main.c:2523`); with
/// extended info where a shell's line carried `-e`.
fn engine(
    ini: &[(Vec<u8>, Vec<u8>)],
    files: IniFiles,
    origin: Origin,
    extended_info: bool,
) -> Engine {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|start| start()).collect();
    let builder = Engine::builder()
        .ini_files(files)
        .sapi_name(NAME)
        .extensions(extensions)
        .configure(php_bundle::CONFIGURE)
        .tier::<php_bundle::Tier>()
        .extended_info(extended_info);
    let builder = natives::declare(builder, origin).skip_shebang_while("cgi.check_shebang_line");
    php_sapi::start(builder, ini, |entry| Process.log(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(words: &[&str], env: &[(&str, &str)]) -> Options {
        let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
        let env = Environment(
            env.iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
        );
        Options::parse(&argv, &env)
    }

    /// The invocation a line comes to, where it comes to a request.
    fn served(options: Options) -> Option<Invocation> {
        match options.ask {
            Ask::Serve(invocation) => Some(invocation),
            _ => None,
        }
    }

    #[test]
    fn a_server_s_environment_leaves_the_line_s_file_unread() {
        let options = parse(
            &["-C", "-n", "-d", "precision=3", "-f", "x.php"],
            &[("REQUEST_METHOD", "GET")],
        );
        assert_eq!(
            options.ini.last(),
            Some(&(b"precision".to_vec(), b"3".to_vec()))
        );
        assert_eq!(
            served(options).map(|i| (i.origin, i.file, i.head, i.no_chdir)),
            Some((Origin::Server, None, Head::Written, false))
        );
    }

    /// `-e` is read by the pass a shell's line alone is given
    /// (`Z/sapi/cgi/cgi_main.c:2307`): from a shell the engine compiles
    /// with extended info, and under a server the switch is never read.
    #[test]
    fn extended_info_is_a_shell_s_switch() {
        let shell = served(parse(&["-e", "t.php"], &[]));
        assert_eq!(
            shell.map(|i| (i.extended_info, i.words)),
            Some((true, vec![b"t.php".to_vec()]))
        );
        let server = served(parse(&["-e", "t.php"], &[("REQUEST_METHOD", "GET")]));
        assert_eq!(server.map(|i| i.extended_info), Some(false));
        let plain = served(parse(&["t.php"], &[]));
        assert_eq!(plain.map(|i| i.extended_info), Some(false));
    }

    #[test]
    fn a_shell_s_line_names_the_file_and_its_words() {
        let invocation = served(parse(&["-q", "-C", "t.php", "a=1", "b"], &[]));
        assert_eq!(
            invocation.map(|i| (i.origin, i.head, i.no_chdir, i.words)),
            Some((
                Origin::Shell,
                Head::Quiet,
                true,
                vec![b"t.php".to_vec(), b"a=1".to_vec(), b"b".to_vec()]
            ))
        );
        let invocation = served(parse(&["-f", "t.php", "x"], &[]));
        assert_eq!(
            invocation.map(|i| (i.file, i.words)),
            Some((Some(b"t.php".to_vec()), vec![b"x".to_vec()]))
        );
        assert_eq!(parse(&["-v"], &[]).ask, Ask::Version);
        assert_eq!(parse(&["-m"], &[]).ask, Ask::Extensions);
        assert_eq!(
            parse(&["-x"], &[]).ask,
            Ask::Usage {
                stderr: "Error in argument 1, char 2: option not found x\n".to_owned(),
                exit_code: 1
            }
        );
        assert_eq!(
            parse(&["-h"], &[]).ask,
            Ask::Usage {
                stderr: String::new(),
                exit_code: 0
            }
        );
    }

    #[test]
    fn a_query_string_of_a_lone_switch_reads_no_line() {
        let invocation = served(parse(&["-s", "x.php"], &[("QUERY_STRING", "%2Ds")]));
        assert_eq!(invocation.map(|i| i.behavior), Some(Behavior::Standard));
    }
}

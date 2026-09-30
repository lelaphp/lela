//! The extension a host registers beside the bundle, php's
//! `additional_module` to `php_module_startup`, and the natives it adds
//! under `standard`, its `sapi_module.additional_functions`
//! (`Z/main/main.c:2359-2366`): `cgi_module_entry`
//! (`Z/sapi/cgi/cgi_main.c:1564-1712`), the `cgi-fcgi` extension php-cgi
//! and the `FastCGI` worker load after every internal one, with its
//! `cgi.*` and `fastcgi.*` entries and the four natives of
//! `cgi_main.stub.php`; and the built-in server's `cli_server` extension with
//! its one entry, and its `server_additional_functions`
//! (`Z/sapi/cli/php_cli_server.c:463-510`). The natives read the
//! request's headers and its header list, which live below the host face,
//! so the extension does too and a host names it with
//! [`EngineBuilder::host_extension`].
//!
//! [`EngineBuilder::host_extension`]: crate::EngineBuilder::host_extension

use php_macros::php_function;
use php_unit::decl::{FunctionDecl, IniDecl, Name};

use crate::api::abi::{Entry, Extension, ExtensionBuilder, Union};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpString, Value};
use crate::engine::HostValue;

/// The extension a host loads after the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostExtension {
    /// `cgi_module_entry`, named `cgi-fcgi`.
    CgiFcgi,
    /// php-fpm's `cgi_module_entry` (`Z/sapi/fpm/fpm/fpm_main.c:1543`),
    /// also named `cgi-fcgi`, with the entries and natives of
    /// `fpm_main.stub.php`.
    FpmFcgi,
    /// php's CLI, which loads no extension of its own and adds its
    /// `additional_functions` under `standard`, the process title's two.
    Cli,
    /// The built-in server's `cli_server_module_entry`, named
    /// `cli_server`, and its natives under `standard`.
    CliServer,
}

impl HostExtension {
    /// `sapi_module.name` of the host that loads it.
    #[must_use]
    pub fn sapi_name(self) -> &'static str {
        match self {
            HostExtension::CgiFcgi => "cgi-fcgi",
            HostExtension::FpmFcgi => "fpm-fcgi",
            HostExtension::Cli => "cli",
            HostExtension::CliServer => "cli-server",
        }
    }
}

/// What a host adds: its extension, and the natives `php_module_startup`
/// registers under `standard` after every extension has started.
pub(crate) struct Hosted {
    pub(crate) extension: Option<Extension>,
    pub(crate) additional: Vec<(FunctionDecl, Entry)>,
}

/// The extension built: `cgi-fcgi`'s natives in the stub's order and
/// its `PHP_INI_BEGIN` rows at their defaults, `None` for php's `NULL`,
/// as `cgi.redirect_status_env`'s; `cli_server` with
/// `cli_server.color` off and nothing else, and its natives beside it.
pub(crate) fn hosted(host: HostExtension) -> Hosted {
    let row = |name: &str, default: Option<&str>, user: bool| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.map(|default| default.as_bytes().into()),
        user,
        per_dir: user,
        system: true,
    };
    match host {
        HostExtension::CgiFcgi => Hosted {
            extension: Some(
                ExtensionBuilder::default()
                    .functions(vec![
                        apache_child_terminate::declared(),
                        apache_request_headers::declared(),
                        getallheaders::declared(),
                        apache_response_headers::declared(),
                    ])
                    .ini(vec![
                        row("cgi.rfc2616_headers", Some("0"), true),
                        row("cgi.nph", Some("0"), true),
                        row("cgi.check_shebang_line", Some("1"), false),
                        row("cgi.force_redirect", Some("1"), false),
                        row("cgi.redirect_status_env", None, false),
                        row("cgi.fix_pathinfo", Some("1"), false),
                        row("cgi.discard_path", Some("0"), false),
                        row("fastcgi.logging", Some("1"), false),
                    ])
                    .build("cgi-fcgi"),
            ),
            additional: Vec::new(),
        },
        HostExtension::FpmFcgi => Hosted {
            extension: Some(
                ExtensionBuilder::default()
                    .functions(vec![
                        fastcgi_finish_request::declared(),
                        apache_request_headers::declared(),
                        getallheaders::declared(),
                        fpm_get_status::declared(),
                    ])
                    .ini(vec![
                        row("cgi.rfc2616_headers", Some("0"), true),
                        row("cgi.nph", Some("0"), true),
                        row("cgi.fix_pathinfo", Some("1"), false),
                        row("cgi.discard_path", Some("0"), false),
                        row("fastcgi.script_path_encoded", Some("1"), false),
                        row("fastcgi.logging", Some("1"), false),
                        row("fastcgi.error_header", None, false),
                        row("fpm.config", None, false),
                    ])
                    .build("cgi-fcgi"),
            ),
            additional: Vec::new(),
        },
        HostExtension::Cli => Hosted {
            extension: None,
            additional: vec![
                cli_set_process_title::declared(),
                cli_get_process_title::declared(),
            ],
        },
        HostExtension::CliServer => Hosted {
            extension: Some(
                ExtensionBuilder::default()
                    .ini(vec![row("cli_server.color", Some("0"), true)])
                    .build("cli_server"),
            ),
            additional: vec![
                cli_set_process_title::declared(),
                cli_get_process_title::declared(),
                apache_request_headers::declared(),
                apache_response_headers::declared(),
                getallheaders::declared(),
            ],
        },
    }
}

/// php-fpm's `fastcgi.error_header` (`Z/sapi/fpm/fpm/fpm_main.c:1934`):
/// a program that ended with exit status 255 before its head went has
/// the entry's line put in, replacing its kind, before the request shuts
/// down. Nothing under a host whose extension has no such entry.
pub(crate) fn after_script(req: &mut Request) {
    if req.exit_status() != 255 || req.output().headers_sent() {
        return;
    }
    if let Some(line) = req
        .ini_get(b"fastcgi.error_header")
        .filter(|line| !line.is_empty())
    {
        let line = line.to_vec();
        let _ = crate::output::header_add(req, &line, true, 0);
    }
}

/// `fastcgi_finish_request(): bool`: where the response is open, every
/// buffer of the request's flushed, the head sent and the connection told the
/// response ends, and `true`; the program runs on, and what it writes
/// after goes nowhere; `false` once the response is closed.
///
/// ```php
/// echo "sent\n";
/// var_dump(fastcgi_finish_request());
/// echo "unheard\n";
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// sent
/// ```
#[php_function]
fn fastcgi_finish_request(req: &mut Request) -> bool {
    req.finish_response()
}

/// `fpm_get_status(): array|false` (`Z/sapi/fpm/fpm/fpm_status.c`
/// `fpm_status_export_to_zval`): the pool's shared table as the host
/// holds it, its counts and each process, or `false` where the host has none.
///
/// ```php
/// $s = fpm_get_status();
/// echo $s['pool'], ' ', $s['process-manager'], ' ', $s['accepted-conn'], ' ',
///     $s['active-processes'], ' ', count($s['procs']), ' ', $s['procs'][0]['state'], "\n";
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// witness static 1 1 1 Running
/// ```
#[php_function]
fn fpm_get_status(req: &mut Request) -> Result<Union<PhpArray, bool>, Thrown> {
    match req.host_status() {
        Some(status) => host_value(req, status).map(Union::A),
        None => Ok(Union::B(false)),
    }
}

/// A host's value made an array: a map's keys strings, a list's the
/// next index.
fn host_value(req: &mut Request, value: HostValue) -> Result<PhpArray, Thrown> {
    let mut array = PhpArray::new(req)?;
    let rows: Vec<(Option<Vec<u8>>, HostValue)> = match value {
        HostValue::Map(rows) => rows.into_iter().map(|(k, v)| (Some(k), v)).collect(),
        HostValue::List(rows) => rows.into_iter().map(|v| (None, v)).collect(),
        other => vec![(None, other)],
    };
    for (key, row) in rows {
        let value = match row {
            HostValue::Int(n) => Value::int(n),
            HostValue::Float(f) => Value::float(f),
            HostValue::Str(s) => Value::string(PhpString::new(req, &s)?),
            nested => Value::array(host_value(req, nested)?),
        };
        match key {
            Some(key) => {
                let key = Key::from_string(PhpString::new(req, &key)?);
                array.set(req, key, value)?;
            }
            None => array.append(req, value)?,
        }
    }
    Ok(array)
}

/// `apache_child_terminate(): void`: `fcgi_terminate` under `FastCGI` and
/// nothing under CGI, which this host is.
///
/// ```php
/// var_dump(apache_child_terminate());
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// NULL
/// ```
#[php_function]
fn apache_child_terminate(req: &mut Request) {
    let _ = req;
}

/// `apache_request_headers(): array`, also `getallheaders()`
/// (`Z/sapi/cgi/cgi_main.c:1576`, `Z/sapi/cli/php_cli_server.c:407`): the
/// request's headers as its host received them. Under CGI that is every
/// `HTTP_` variable of the environment in its order, which the host's is,
/// the name after the prefix kept upper case where it starts and after
/// each `_`, which becomes `-`, and lower case elsewhere, and
/// `CONTENT_TYPE` and `CONTENT_LENGTH` under their header names; under the
/// built-in server, each name as the client spelt it, a name repeated in
/// one spelling once with the values joined:
///
/// ```php
/// print_r(apache_request_headers());
/// var_dump(getallheaders() === apache_request_headers());
/// ```
/// ```request
/// --ENV--
/// HTTP_X_FORWARDED_FOR=10.0.0.1
/// HTTP_X__TEST=a
/// HTTP_X_=b
/// --COOKIE--
/// c=v
/// --POST--
/// p=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// Array
/// (
///     [Content-Length] => 3
///     [Content-Type] => application/x-www-form-urlencoded
///     [Cookie] => c=v
///     [X-] => b
///     [X-Forwarded-For] => 10.0.0.1
///     [X-_test] => a
/// )
/// bool(true)
/// ```
#[php_function(aliases(getallheaders))]
fn apache_request_headers(req: &mut Request) -> Result<PhpArray, Thrown> {
    req.request_headers()
}

/// `apache_response_headers(): array` (`Z/sapi/cgi/cgi_main.c:1689`):
/// each line of the header list with a name before its colon, the name
/// with its trailing blanks off and the value with its leading blanks
/// off; a line with no colon or an empty name is passed over, and the
/// head php-cgi writes carries it as given.
///
/// ```php
/// header("X-Robots-Tag : noindex");
/// header("Bad-header");
/// header(" : ");
/// print_r(apache_response_headers());
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// X-Robots-Tag : noindex
/// Bad-header
///  :
/// Content-type: text/html; charset=UTF-8
///
/// Array
/// (
///     [X-Powered-By] => PHP/8.5.9
///     [X-Robots-Tag] => noindex
/// )
/// ```
#[php_function]
fn apache_response_headers(req: &mut Request) -> Result<PhpArray, Thrown> {
    let lines = req.output().headers().to_vec();
    let mut array = PhpArray::new(req)?;
    for line in lines {
        let Some(colon) = line.iter().position(|&b| b == b':') else {
            continue;
        };
        let mut end = colon;
        while end > 0 && matches!(line[end - 1], b' ' | b'\t') {
            end -= 1;
        }
        if end == 0 {
            continue;
        }
        let mut start = colon + 1;
        while matches!(line.get(start), Some(b' ' | b'\t')) {
            start += 1;
        }
        let key = Key::from_string(PhpString::new(req, &line[..end])?);
        let value = Value::string(PhpString::new(req, &line[start..])?);
        array.set(req, key, value)?;
    }
    Ok(array)
}

/// `cli_set_process_title(string $title): bool`
/// (`Z/sapi/cli/php_cli_process_title.c:26`): the title `ps` shows set
/// over the host's area, and `true`; a title as long as the area or
/// longer, or no area saved, is `set_ps_title`'s error in php's warning,
/// and `false`. Before any title is set the one read is empty.
///
/// ```php
/// var_dump(PHP_CLI_PROCESS_TITLE, cli_get_process_title());
/// var_dump(cli_set_process_title("witness title"), cli_get_process_title());
/// var_dump(cli_set_process_title(str_repeat("x", 100000)), cli_get_process_title());
/// ```
/// ```expect process
/// bool(true)
/// string(0) ""
/// bool(true)
/// string(13) "witness title"
///
/// Warning: cli_set_process_title(): cli_set_process_title had an error: Too long in %s on line 4
/// bool(false)
/// string(13) "witness title"
/// ```
#[php_function]
fn cli_set_process_title(req: &mut Request, title: PhpString) -> bool {
    // With no area `ps_buffer_size` is zero, which every title meets.
    let set = req
        .process_title()
        .map_or(Err("Too long"), |area| area.set(title.as_bytes()));
    match set {
        Ok(()) => true,
        Err(why) => {
            crate::api::raising::docref(
                req,
                crate::api::raising::Level::Warning,
                &format!("cli_set_process_title had an error: {why}"),
            );
            false
        }
    }
}

/// `cli_get_process_title(): ?string`
/// (`Z/sapi/cli/php_cli_process_title.c:47`): the title last set, or
/// `null` with php's warning where no area was saved; see
/// [`cli_set_process_title`].
///
/// ```php
/// cli_set_process_title("again");
/// var_dump(cli_get_process_title());
/// ```
/// ```expect process
/// string(5) "again"
/// ```
#[php_function]
fn cli_get_process_title(req: &mut Request) -> Result<Option<PhpString>, Thrown> {
    let Some(title) = req.process_title().map(crate::engine::ProcessTitle::get) else {
        crate::api::raising::docref(
            req,
            crate::api::raising::Level::Warning,
            "cli_get_process_title had an error: Not initialized correctly",
        );
        return Ok(None);
    };
    PhpString::new(req, &title).map(Some)
}

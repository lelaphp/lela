//! What php-fpm hands `php_module_startup` beside the bundle, declared
//! here over `php_ext` as an extension declares its own (ADR-0008 as
//! amended by #1348): its `cgi_module_entry`
//! (`Z/sapi/fpm/fpm/fpm_main.c:1543`), named `cgi-fcgi` as php-cgi's is,
//! with the entries and natives of `fpm_main.stub.php`, and the line
//! `fastcgi.error_header` puts into the head of a program that failed
//! before its head went. php-fpm names no `additional_functions`.
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{Extension, ExtensionBuilder, Union};
use php_ext::attr::php_function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpArray;

/// One `PHP_INI_BEGIN` row at its default, `None` for php's `NULL`; `user`
/// is `PHP_INI_ALL`, otherwise `PHP_INI_SYSTEM`.
fn row(name: &str, default: Option<&str>, user: bool) -> IniDecl {
    IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.map(|default| default.as_bytes().into()),
        user,
        per_dir: user,
        system: true,
    }
}

/// php-fpm's `cgi_module_entry`: its natives in the stub's order, its
/// `PHP_INI_BEGIN` rows, and the error header after the program.
#[must_use]
pub(crate) fn extension() -> Extension {
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
        .program_ended(error_header)
        .build("cgi-fcgi")
}

/// `fastcgi.error_header` (`Z/sapi/fpm/fpm/fpm_main.c:1934`): a program
/// that ended with exit status 255 before its head went has the entry's
/// line put in, replacing its kind, before the request shuts down.
fn error_header(req: &mut Request) {
    if req.exit_status() != 255 || req.headers_sent().is_some() {
        return;
    }
    if let Some(line) = req
        .ini_get(b"fastcgi.error_header")
        .filter(|line| !line.is_empty())
    {
        let line = line.to_vec();
        let _ = req.header_add(&line, true, 0);
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
/// `fpm_status_export_to_zval`): the pool's shared table as this host
/// holds it, its counts and each process, or `false` where it has none.
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
    Ok(match req.host_status()? {
        Some(status) => Union::A(status),
        None => Union::B(false),
    })
}

/// `apache_request_headers(): array`, also `getallheaders()`
/// (`Z/sapi/fpm/fpm/fpm_main.c:1452`): the request's headers as the
/// server sent them in its parameters, in the order it sent them, each
/// `HTTP_` one with the name after the prefix kept upper case where it
/// starts and after each `_`, which becomes `-`, and lower case
/// elsewhere, and `CONTENT_TYPE` and `CONTENT_LENGTH` under their header
/// names.
///
/// ```php
/// print_r(apache_request_headers());
/// var_dump(getallheaders() === apache_request_headers());
/// ```
/// ```request fpm
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
///     [Cookie] => c=v
///     [Content-Length] => 3
///     [Content-Type] => application/x-www-form-urlencoded
/// )
/// bool(true)
/// ```
#[php_function(aliases(getallheaders))]
fn apache_request_headers(req: &mut Request) -> Result<PhpArray, Thrown> {
    req.request_headers()
}

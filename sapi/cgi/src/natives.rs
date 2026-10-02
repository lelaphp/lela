//! What php-cgi hands `php_module_startup` beside the bundle, declared
//! here over `php_ext` as an extension declares its own (ADR-0008 as
//! amended by #1348): `cgi_module_entry` (`Z/sapi/cgi/cgi_main.c:1564-1712`),
//! the extension named `cgi-fcgi` it loads after every internal one, with
//! its `cgi.*` and `fastcgi.*` entries and the four natives of
//! `cgi_main.stub.php`; and, where a shell started it, its
//! `additional_functions`, `dl()` under `standard`
//! (`Z/sapi/cgi/cgi_main.c:1009-1012`, `:1874`).
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString};
use php_sapi::EngineBuilder;

use crate::Origin;

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

/// `builder` with what php-cgi declares itself: `cgi_module_entry` as its
/// `additional_module`, and its `additional_functions`, which `main` hands
/// php only where neither the environment nor the line made php-cgi CGI
/// or `FastCGI` (`Z/sapi/cgi/cgi_main.c:1874`): `dl()` started from a
/// shell, nothing under a server.
#[must_use]
pub fn declare(builder: EngineBuilder, origin: Origin) -> EngineBuilder {
    let builder = builder.additional_extension(extension());
    match origin {
        Origin::Shell => builder.additional_functions(vec![dl::declared()]),
        Origin::Server => builder,
    }
}

/// `cgi_module_entry`: its natives in the stub's order and its
/// `PHP_INI_BEGIN` rows, `cgi.redirect_status_env`'s default php's `NULL`.
fn extension() -> Extension {
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
        .build(crate::NAME)
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
/// (`Z/sapi/cgi/cgi_main.c:1576`): the request's headers as the server
/// described them, every `HTTP_` variable of the environment in its
/// order, the name after the prefix kept upper case where it starts and
/// after each `_`, which becomes `-`, and lower case elsewhere, and
/// `CONTENT_TYPE` and `CONTENT_LENGTH` under their header names:
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
/// the header list as name and value ([`Request::response_headers`]),
/// and the head php-cgi writes carries each line as given.
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
    req.response_headers()
}

/// `dl(string $extension_filename): bool`, `ext/standard`'s, which php-cgi
/// started from a shell names in its `additional_functions`
/// ([`Request::dl`]); under a server php-cgi declares none, so the fence,
/// which a server serves, finds no `dl()`. The shell's is
/// `tests/shell.rs`'s `dl_is_declared_from_a_shell`.
///
/// ```php
/// var_dump(function_exists("dl"));
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// bool(false)
/// ```
#[php_function]
fn dl(req: &mut Request, extension_filename: PhpString) -> bool {
    req.dl(extension_filename.as_bytes())
}

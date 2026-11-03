//! What makala hands `php_module_startup` beside the bundle, declared over
//! `php_ext` as an extension declares its own (ADR-0008 as amended by
//! #1348): php-fpm's natives as #2335 chose them, `fastcgi_finish_request`
//! and `getallheaders`, which php-fpm declares as the alias of
//! `apache_request_headers` and so declares here, so a framework that
//! asks `function_exists()` for any of them finds it as under php-fpm,
//! and Symfony's `Response::send()` ends the response early; php-fpm's
//! `fpm_get_status` reads its process manager, which this host has none
//! of. They are the host's `additional_functions`, which php registers
//! under `standard`, as `php -S` declares its `getallheaders`
//! (`server_additional_functions`, `Z/sapi/cli/php_cli_server.c:503-510`).
//! Each is one call into what the engine holds once for every host that
//! declares it, `Response::finish_response` and `Variables::request_headers`,
//! which php-fpm's and `php -S`'s declarations call too: a declaration is
//! each host's, as each `sapi/` directory's is in php-src, and no crate both hosts may
//! depend on declares natives (ADR-0008).
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray};
use php_sapi::EngineBuilder;

/// `builder` with the host's natives.
#[must_use]
pub(crate) fn declare(builder: EngineBuilder) -> EngineBuilder {
    builder.additional_functions(&[
        fastcgi_finish_request::DECLARED,
        apache_request_headers::DECLARED,
        getallheaders::DECLARED,
    ])
}

/// `fastcgi_finish_request(): bool` (`Z/sapi/fpm/fpm/fpm_main.c:1494`):
/// where the response is open, every buffer flushed, the head sent and
/// the response ended, and `true`; the program runs on, and what it
/// writes after goes nowhere; `false` once the response is closed.
///
/// ```php
/// echo "sent\n";
/// var_dump(fastcgi_finish_request());
/// echo "unheard\n";
/// ```
/// ```request makala
/// --GET--
/// q=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.12
/// content-type: text/html; charset=UTF-8
///
/// sent
/// ```
#[php_function]
fn fastcgi_finish_request(req: &mut Request) -> bool {
    req.response_mut().finish_response()
}

/// `apache_request_headers(): array`, also `getallheaders()`
/// (`Z/sapi/fpm/fpm/fpm_main.c:1452`): the
/// request's headers as php-fpm reads them from the server's parameters,
/// newest first, each `HTTP_` name with the prefix gone, `_` made `-`
/// and each word capitalised, and `CONTENT_TYPE` and `CONTENT_LENGTH`
/// under their header names.
///
/// ```php
/// print_r(apache_request_headers());
/// var_dump(getallheaders() === apache_request_headers());
/// ```
/// ```request makala
/// --COOKIE--
/// c=v
/// --POST--
/// p=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.12
/// content-type: text/html; charset=UTF-8
///
/// Array
/// (
///     [Content-Length] => 3
///     [Content-Type] => application/x-www-form-urlencoded
///     [Cookie] => c=v
///     [Host] => localhost
/// )
/// bool(true)
/// ```
#[php_function(aliases(getallheaders))]
fn apache_request_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.variables_mut().request_headers()
}

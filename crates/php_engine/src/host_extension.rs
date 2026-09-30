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

use crate::api::abi::{Entry, Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpString, Value};

/// The extension a host loads after the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostExtension {
    /// `cgi_module_entry`, named `cgi-fcgi`.
    CgiFcgi,
    /// The built-in server's `cli_server_module_entry`, named
    /// `cli_server`, and its natives under `standard`.
    CliServer,
}

/// What a host adds: its extension, and the natives `php_module_startup`
/// registers under `standard` after every extension has started.
pub(crate) struct Hosted {
    pub(crate) extension: Extension,
    pub(crate) additional: Vec<(FunctionDecl, Entry)>,
}

/// The extension built: `cgi-fcgi`'s natives in the stub's order and
/// its `PHP_INI_BEGIN` rows at their defaults, `cgi.redirect_status_env`'s
/// `NULL` read as empty, as `ini_get` answers it; `cli_server` with
/// `cli_server.color` off and nothing else, and its natives beside it.
pub(crate) fn hosted(host: HostExtension) -> Hosted {
    let row = |name: &str, default: &str, user: bool| IniDecl {
        name: Name(name.as_bytes().into()),
        default: default.as_bytes().into(),
        user,
        per_dir: user,
        system: true,
    };
    match host {
        HostExtension::CgiFcgi => Hosted {
            extension: ExtensionBuilder::default()
                .functions(vec![
                    apache_child_terminate::declared(),
                    apache_request_headers::declared(),
                    getallheaders::declared(),
                    apache_response_headers::declared(),
                ])
                .ini(vec![
                    row("cgi.rfc2616_headers", "0", true),
                    row("cgi.nph", "0", true),
                    row("cgi.check_shebang_line", "1", false),
                    row("cgi.force_redirect", "1", false),
                    row("cgi.redirect_status_env", "", false),
                    row("cgi.fix_pathinfo", "1", false),
                    row("cgi.discard_path", "0", false),
                    row("fastcgi.logging", "1", false),
                ])
                .build("cgi-fcgi"),
            additional: Vec::new(),
        },
        HostExtension::CliServer => Hosted {
            extension: ExtensionBuilder::default()
                .ini(vec![row("cli_server.color", "0", true)])
                .build("cli_server"),
            additional: vec![
                apache_request_headers::declared(),
                apache_response_headers::declared(),
                getallheaders::declared(),
            ],
        },
    }
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

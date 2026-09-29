//! The extension a host registers beside the bundle, php's
//! `additional_module` to `php_module_startup`: `cgi_module_entry`
//! (`Z/sapi/cgi/cgi_main.c:1564-1712`), the `cgi-fcgi` extension php-cgi
//! and the `FastCGI` worker load after every internal one, with its
//! `cgi.*` and `fastcgi.*` entries and the four natives of
//! `cgi_main.stub.php`. The natives read the request's environment and
//! its header list, which live below the host face, so the extension does
//! too and a host names it with [`EngineBuilder::host_extension`].
//!
//! [`EngineBuilder::host_extension`]: crate::EngineBuilder::host_extension

use php_macros::php_function;
use php_unit::decl::{IniDecl, Name};

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpString, Value};

/// The extension a host loads after the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostExtension {
    /// `cgi_module_entry`, named `cgi-fcgi`.
    CgiFcgi,
}

/// The extension built: `cgi-fcgi`'s natives in the stub's order and
/// its `PHP_INI_BEGIN` rows at their defaults, `cgi.redirect_status_env`'s
/// `NULL` read as empty, as `ini_get` answers it.
pub(crate) fn extension(host: HostExtension) -> Extension {
    match host {
        HostExtension::CgiFcgi => {
            let row = |name: &str, default: &str, user: bool| IniDecl {
                name: Name(name.as_bytes().into()),
                default: default.as_bytes().into(),
                user,
                per_dir: user,
                system: true,
            };
            ExtensionBuilder::default()
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
                .build("cgi-fcgi")
        }
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
/// (`Z/sapi/cgi/cgi_main.c:1576`): under CGI, every `HTTP_` variable of
/// the environment in its order, which the host's is, the name after the prefix kept upper
/// case where it starts and after each `_`, which becomes `-`, and lower
/// case elsewhere, and `CONTENT_TYPE` and `CONTENT_LENGTH` under their
/// header names:
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
    let pairs = req.env_pairs().to_vec();
    let mut array = PhpArray::new(req)?;
    for (name, value) in pairs {
        let name = if let Some(rest) = name.strip_prefix(b"HTTP_").filter(|r| !r.is_empty()) {
            header_name(rest)
        } else if name == b"CONTENT_TYPE" {
            b"Content-Type".to_vec()
        } else if name == b"CONTENT_LENGTH" {
            b"Content-Length".to_vec()
        } else {
            continue;
        };
        let key = Key::from_string(PhpString::new(req, &name)?);
        let value = Value::string(PhpString::new(req, &value)?);
        array.set(req, key, value)?;
    }
    Ok(array)
}

/// The header name an `HTTP_` variable's rest spells: its first byte as
/// it stands, each `_` a `-` with the byte after it as it stands, and
/// every other upper case letter lowered.
fn header_name(rest: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(rest.len());
    let mut bytes = rest.iter().copied();
    if let Some(first) = bytes.next() {
        name.push(first);
    }
    while let Some(byte) = bytes.next() {
        if byte == b'_' {
            name.push(b'-');
            if let Some(next) = bytes.next() {
                name.push(next);
            }
        } else {
            name.push(byte.to_ascii_lowercase());
        }
    }
    name
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

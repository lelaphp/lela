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
use php_ext::values::{Owner, PhpArray, PhpString};
use php_sapi::EngineBuilder;

use crate::Origin;

/// One `PHP_INI_BEGIN` row: its name, its default, `None` for php's
/// `NULL`, whether it is `PHP_INI_ALL` rather than `PHP_INI_SYSTEM`, and
/// whether it is a `STD_PHP_INI_BOOLEAN`, whose displayer is
/// `zend_ini_boolean_displayer_cb`.
struct Row {
    name: &'static str,
    default: Option<&'static str>,
    user: bool,
    boolean: bool,
}

/// `cgi_main.c`'s `PHP_INI_BEGIN` (`Z/sapi/cgi/cgi_main.c:1504-1516`),
/// less `fastcgi.impersonate`, which only Windows declares.
const INI: [Row; 8] = [
    Row::boolean("cgi.rfc2616_headers", "0", ALL),
    Row::boolean("cgi.nph", "0", ALL),
    Row::boolean("cgi.check_shebang_line", "1", SYSTEM),
    Row::boolean("cgi.force_redirect", "1", SYSTEM),
    Row::entry("cgi.redirect_status_env", SYSTEM),
    Row::boolean("cgi.fix_pathinfo", "1", SYSTEM),
    Row::boolean("cgi.discard_path", "0", SYSTEM),
    Row::boolean("fastcgi.logging", "1", SYSTEM),
];

/// `PHP_INI_ALL`, which a program may change.
const ALL: bool = true;
/// `PHP_INI_SYSTEM`, which only the host's configuration sets.
const SYSTEM: bool = false;

impl Row {
    /// `STD_PHP_INI_BOOLEAN(name, default, modifiable)`.
    const fn boolean(name: &'static str, default: &'static str, user: bool) -> Row {
        Row {
            name,
            default: Some(default),
            user,
            boolean: true,
        }
    }

    /// `STD_PHP_INI_ENTRY(name, NULL, modifiable)`, a string with no
    /// displayer.
    const fn entry(name: &'static str, user: bool) -> Row {
        Row {
            name,
            default: None,
            user,
            boolean: false,
        }
    }

    /// The row as the engine declares it.
    fn decl(&self) -> IniDecl {
        IniDecl {
            name: Name(self.name.as_bytes().into()),
            default: self.default.map(|default| default.as_bytes().into()),
            user: self.user,
            per_dir: self.user,
            system: true,
        }
    }
}

/// `PHP_MINFO_FUNCTION(cgi)` (`Z/sapi/cgi/cgi_main.c:1558`):
/// `DISPLAY_INI_ENTRIES()` alone, a boolean entry `On` or `Off` in both
/// columns, so php-cgi writes no `Version` row.
///
/// ```php
/// ini_set('cgi.rfc2616_headers', 'yes');
/// (new ReflectionExtension('cgi-fcgi'))->info();
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// <h2><a name="module_cgi-fcgi" href="#module_cgi-fcgi">cgi-fcgi</a></h2>
/// <table>
/// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
/// <tr><td class="e">cgi.rfc2616_headers</td><td class="v">On</td><td class="v">Off</td></tr>
/// <tr><td class="e">cgi.nph</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">cgi.check_shebang_line</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">cgi.force_redirect</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">cgi.redirect_status_env</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">cgi.fix_pathinfo</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">cgi.discard_path</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">fastcgi.logging</td><td class="v">On</td><td class="v">On</td></tr>
/// </table>
/// ```
fn info(req: &mut Request) {
    let entries = req.ini_entries();
    let shown = |row: &Row, value: Option<&[u8]>| -> String {
        let value = value.unwrap_or_default();
        match row.boolean {
            true if Request::ini_parse_bool(value) => String::from("On"),
            true => String::from("Off"),
            false => String::from_utf8_lossy(value).into_owned(),
        }
    };
    let rows: Vec<[String; 3]> = INI
        .iter()
        .filter_map(|row| {
            let entry = entries
                .iter()
                .find(|entry| entry.decl.name.0.as_ref() == row.name.as_bytes())?;
            Some([
                row.name.to_owned(),
                shown(row, entry.value.as_deref()),
                shown(row, entry.master.as_deref()),
            ])
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.info_ini_table(&cells);
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
/// `PHP_INI_BEGIN` rows, `cgi.redirect_status_env`'s default php's `NULL`,
/// and its `MINFO`.
fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            apache_child_terminate::declared(),
            apache_request_headers::declared(),
            getallheaders::declared(),
            apache_response_headers::declared(),
        ])
        .ini(INI.iter().map(Row::decl).collect())
        .info(info)
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
fn apache_request_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.variables_mut().request_headers()
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
fn apache_response_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
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

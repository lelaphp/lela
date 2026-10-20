//! What php-fpm hands `php_module_startup` beside the bundle, declared
//! here over `php_ext` as an extension declares its own (ADR-0008 as
//! amended by #1348): its `cgi_module_entry`
//! (`Z/sapi/fpm/fpm/fpm_main.c:1543`), named `cgi-fcgi` as php-cgi's is,
//! with the entries and natives of `fpm_main.stub.php`, and the line
//! `fastcgi.error_header` puts into the head of a program that failed
//! before its head went. php-fpm names no `additional_functions`.
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{Extension, ExtensionBuilder, OrFalse};
use php_ext::attr::php_function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpArray};

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

/// `fpm_main.c`'s `PHP_INI_BEGIN` (`Z/sapi/fpm/fpm/fpm_main.c:1433-1442`).
const INI: [Row; 8] = [
    Row::boolean("cgi.rfc2616_headers", "0", ALL),
    Row::boolean("cgi.nph", "0", ALL),
    Row::boolean("cgi.fix_pathinfo", "1", SYSTEM),
    Row::boolean("cgi.discard_path", "0", SYSTEM),
    Row::boolean("fastcgi.script_path_encoded", "1", SYSTEM),
    Row::boolean("fastcgi.logging", "1", SYSTEM),
    Row::entry("fastcgi.error_header", SYSTEM),
    Row::entry("fpm.config", SYSTEM),
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

/// `PHP_MINFO_FUNCTION(cgi)` (`Z/sapi/fpm/fpm/fpm_main.c:1484`): a table
/// of one row, `php-fpm` `active`, then `DISPLAY_INI_ENTRIES()`, a
/// boolean entry `On` or `Off` in both columns.
///
/// ```php
/// ini_set('cgi.rfc2616_headers', 'yes');
/// (new ReflectionExtension('cgi-fcgi'))->info();
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// <h2><a name="module_cgi-fcgi" href="#module_cgi-fcgi">cgi-fcgi</a></h2>
/// <table>
/// <tr><td class="e">php-fpm </td><td class="v">active </td></tr>
/// </table>
/// <table>
/// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
/// <tr><td class="e">cgi.rfc2616_headers</td><td class="v">On</td><td class="v">Off</td></tr>
/// <tr><td class="e">cgi.nph</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">cgi.fix_pathinfo</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">cgi.discard_path</td><td class="v">Off</td><td class="v">Off</td></tr>
/// <tr><td class="e">fastcgi.script_path_encoded</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">fastcgi.logging</td><td class="v">On</td><td class="v">On</td></tr>
/// <tr><td class="e">fastcgi.error_header</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// <tr><td class="e">fpm.config</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
/// </table>
/// ```
fn info(req: &mut Request) {
    req.extensions_mut().info_table(&[&["php-fpm", "active"]]);
    let entries = req.ini().entries();
    let shown = |row: &Row, value: Option<&[u8]>| -> String {
        let value = value.unwrap_or_default();
        match row.boolean {
            true if Ini::parse_bool(value) => String::from("On"),
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
    req.ini_mut().info_ini_table(&cells);
}

/// php-fpm's `cgi_module_entry`: its natives in the stub's order, its
/// `PHP_INI_BEGIN` rows, its `MINFO`, and the error header after the
/// program.
#[must_use]
pub(crate) fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            fastcgi_finish_request::declared(),
            apache_request_headers::declared(),
            getallheaders::declared(),
            fpm_get_status::declared(),
        ])
        .ini(INI.iter().map(Row::decl).collect())
        .info(info)
        .program_ended(error_header)
        .build("cgi-fcgi")
}

/// `fastcgi.error_header` (`Z/sapi/fpm/fpm/fpm_main.c:1934`): a program
/// that ended with exit status 255 before its head went has the entry's
/// line put in, replacing its kind, before the request shuts down.
fn error_header(req: &mut Request) {
    if req.diagnostics().exit_status() != 255 || req.response().headers_sent().is_some() {
        return;
    }
    if let Some(line) = req
        .ini()
        .get(b"fastcgi.error_header")
        .filter(|line| !line.is_empty())
    {
        let line = line.to_vec();
        let _ = req.response_mut().header_add(&line, true, 0);
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
    req.response_mut().finish_response()
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
///
/// Its return is declared as the stub declares it, `array|false`:
///
/// ```php
/// echo (new ReflectionFunction('fpm_get_status'))->getReturnType(), "\n";
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// array|false
/// ```
#[php_function]
fn fpm_get_status(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    Ok(match req.response_mut().host_status()? {
        Some(status) => OrFalse::Value(status),
        None => OrFalse::False,
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
fn apache_request_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.variables_mut().request_headers()
}

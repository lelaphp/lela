//! `Z/ext/standard/info.c` and `credits.c`: `phpinfo`, `phpversion`,
//! `phpcredits`, `php_sapi_name`, `php_uname`, `php_ini_scanned_files`
//! and `php_ini_loaded_file`, and the `INFO_*` and `CREDITS_*`
//! constants. The page is the text form `phpinfo_as_text` prints, the
//! CLI's; the general table states what this engine is: its version,
//! the host's `uname(2)`, the server API, that no ini file was read and
//! the wrappers it registered.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::fmt::Write as _;
use std::ops::ControlFlow;

use php_ext::abi::{OrFalse, True};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::Thrown;
use php_ext::request::{InfoRow, PHP_VERSION, Request, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpString};

use crate::arg_value_error;

mod logos;
use crate::string::Decimal;
use constants::{
    CREDITS_ALL, CREDITS_DOCS, CREDITS_FULLPAGE, CREDITS_GENERAL, CREDITS_GROUP, CREDITS_MODULES,
    CREDITS_QA, CREDITS_SAPI, INFO_CONFIGURATION, INFO_CREDITS, INFO_ENVIRONMENT, INFO_GENERAL,
    INFO_LICENSE, INFO_MODULES, INFO_VARIABLES,
};

/// `PHP_CREDITS_WEB`, `Z/ext/standard/credits.h:32`, which the stub does
/// not declare.
const CREDITS_WEB: i64 = 1 << 7;

/// The constants, in the stub's order.
///
/// ```php
/// var_dump(INFO_GENERAL, INFO_CREDITS, INFO_CONFIGURATION, INFO_MODULES, INFO_ENVIRONMENT, INFO_VARIABLES, INFO_LICENSE, INFO_ALL);
/// var_dump(CREDITS_GROUP, CREDITS_GENERAL, CREDITS_SAPI, CREDITS_MODULES, CREDITS_DOCS, CREDITS_FULLPAGE, CREDITS_QA, CREDITS_ALL);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(16)
/// int(32)
/// int(64)
/// int(4294967295)
/// int(1)
/// int(2)
/// int(4)
/// int(8)
/// int(16)
/// int(32)
/// int(64)
/// int(4294967295)
/// ```
#[php_const]
pub(crate) mod constants {
    /// `PHP_INFO_*`, `Z/ext/standard/info.h:26`.
    pub(crate) const INFO_GENERAL: i64 = 1;
    pub(crate) const INFO_CREDITS: i64 = 1 << 1;
    pub(crate) const INFO_CONFIGURATION: i64 = 1 << 2;
    pub(crate) const INFO_MODULES: i64 = 1 << 3;
    pub(crate) const INFO_ENVIRONMENT: i64 = 1 << 4;
    pub(crate) const INFO_VARIABLES: i64 = 1 << 5;
    pub(crate) const INFO_LICENSE: i64 = 1 << 6;
    pub(crate) const INFO_ALL: i64 = 0xFFFF_FFFF;
    /// `PHP_CREDITS_*`, `Z/ext/standard/credits.h:24`.
    pub(crate) const CREDITS_GROUP: i64 = 1;
    pub(crate) const CREDITS_GENERAL: i64 = 1 << 1;
    pub(crate) const CREDITS_SAPI: i64 = 1 << 2;
    pub(crate) const CREDITS_MODULES: i64 = 1 << 3;
    pub(crate) const CREDITS_DOCS: i64 = 1 << 4;
    pub(crate) const CREDITS_FULLPAGE: i64 = 1 << 5;
    pub(crate) const CREDITS_QA: i64 = 1 << 6;
    pub(crate) const CREDITS_ALL: i64 = 0xFFFF_FFFF;
}

/// `php_info_print`: the text as it is.
fn print(req: &mut Request, text: &str) {
    req.response_mut().write(text.as_bytes());
}

/// `!sapi_module.phpinfo_as_text`: whether the page is `info.c`'s HTML,
/// which a host serving the web prints, or its text, the CLI's.
fn html(req: &Request) -> bool {
    !req.extensions().phpinfo_as_text()
}

/// `php_info_print_html_esc`: `php_escape_html_entities(str, len, 0,
/// ENT_QUOTES, "utf-8")`.
fn esc(req: &mut Request, text: &[u8]) -> Vec<u8> {
    crate::html::escape(req, text, false, crate::html::QUOTES, Some(b"utf-8"), true)
}

/// `SECTION(name)`: an `<h2>` in HTML, a one-column table's header in
/// text.
fn section(req: &mut Request, name: &str) {
    if html(req) {
        print(req, &format!("<h2>{name}</h2>\n"));
    } else {
        req.extensions_mut().info_rows(&[InfoRow::Header(&[name])]);
    }
}

/// `php_info_print_hr`.
fn hr(req: &mut Request) {
    if html(req) {
        print(req, "<hr />\n");
    } else {
        print(
            req,
            "\n\n _______________________________________________________________________\n\n",
        );
    }
}

/// `php_info_print_box_start(0)`, `contents` and `php_info_print_box_end`:
/// a table of one `class="v"` cell in HTML, a blank line more in text.
fn plain_box(req: &mut Request, contents: &[u8]) {
    let mut boxed = Vec::new();
    if html(req) {
        boxed.extend_from_slice(b"<tr class=\"v\"><td>\n");
        boxed.extend_from_slice(contents);
        boxed.extend_from_slice(b"</td></tr>\n");
    } else {
        boxed.push(b'\n');
        boxed.extend_from_slice(contents);
    }
    req.extensions_mut().info_rows(&[InfoRow::Written(&boxed)]);
}

/// `php_get_uname(mode)` (`Z/ext/standard/info.c:655`): one field of
/// `uname(2)`, or all five with a space between for `a`.
fn get_uname(mode: u8) -> Vec<u8> {
    let Some([sysname, nodename, release, version, machine]) = crate::ffi::uname() else {
        return b"Darwin".to_vec();
    };
    match mode {
        b's' => sysname,
        b'n' => nodename,
        b'r' => release,
        b'v' => version,
        b'm' => machine,
        _ => [sysname, nodename, release, version, machine].join(&b' '),
    }
}

/// `sapi_module.pretty_name` for the hosts this engine serves.
fn pretty_name(name: &str) -> &str {
    match name {
        "cli" => "Command Line Interface",
        "cgi-fcgi" => "CGI/FastCGI",
        "fpm-fcgi" => "FPM/FastCGI",
        "embed" => "PHP Embedded Library",
        other => other,
    }
}

/// `php_info_print_stream_hash`: `Registered <name>` and the names with
/// `, ` between, a row of its own that closes no line in text and whose
/// cells are not closed by a space in HTML; a `none registered` row
/// where there are none.
fn stream_hash(req: &mut Request, name: &str, names: &[Vec<u8>]) -> Vec<u8> {
    let heading = format!("Registered {name}");
    if names.is_empty() {
        // Rendered where it stands, between the general table's rows.
        let mut row = Vec::new();
        let none = "none registered";
        if html(req) {
            row.extend_from_slice(
                format!("<tr><td class=\"e\">{heading} </td><td class=\"v\">{none} </td></tr>\n")
                    .as_bytes(),
            );
        } else {
            row.extend_from_slice(format!("{heading} => {none}\n").as_bytes());
        }
        return row;
    }
    let mut out = Vec::new();
    if html(req) {
        out.extend_from_slice(
            format!("<tr><td class=\"e\">{heading}</td><td class=\"v\">").as_bytes(),
        );
    } else {
        out.extend_from_slice(format!("\n{heading} => ").as_bytes());
    }
    for (i, key) in names.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        if html(req) {
            let escaped = esc(req, key);
            out.extend_from_slice(&escaped);
        } else {
            out.extend_from_slice(key);
        }
    }
    if html(req) {
        out.extend_from_slice(b"</td></tr>\n");
    }
    out
}

/// `get_zend_version()`: the engine's line and one for each Zend
/// extension loaded, as `zend_append_version_info` appends it.
fn zend_version(req: &Request) -> String {
    let mut text = format!(
        "Zend Engine v4.{}, Copyright (c) Zend Technologies\n",
        &PHP_VERSION[2..]
    );
    for zend in req.extensions().zend_extensions() {
        writeln!(
            text,
            "    with {} v{}, {}, by {}",
            zend.name, zend.version, zend.copyright, zend.author
        )
        .ok();
    }
    text
}

/// The general table of `php_print_info` (`Z/ext/standard/info.c:766`):
/// the version, in HTML inside the box with php's logo, the table of
/// what this build is, and the box naming the engine. The build's own
/// facts, its date, the target it was built for, its provider where it
/// named one and the bundle's `configure` switches, are the engine's
/// `BuildDefs`, the one stamp `-v` and `PHP_BUILD_DATE` read too; the rows
/// after them say what this engine is. `Zend Multibyte Support` names
/// the extension that set the encoding functions, `provided by %s` of
/// its `provider_name`, and is `disabled` where none did
/// (`Z/ext/standard/info.c:872-881`):
///
/// ```php
/// ob_start(); phpinfo(INFO_GENERAL); preg_match('/^Zend Multibyte Support => .*$/m', ob_get_clean(), $row);
/// echo $row[0], "\n";
/// ```
/// ```expect
/// Zend Multibyte Support => provided by mbstring
/// ```
fn general(req: &mut Request) {
    if html(req) {
        let logo = if crate::ffi::local_month_day() == Some((3, 1)) {
            logos::PHP_EGG_LOGO
        } else {
            logos::PHP_LOGO
        };
        let boxed = format!(
            "<tr class=\"h\"><td>\n<a href=\"https://www.php.net/\"><img src=\"{logo}\" alt=\"PHP logo\" /></a><h1 class=\"p\">PHP Version {PHP_VERSION}</h1>\n</td></tr>\n"
        );
        req.extensions_mut()
            .info_rows(&[InfoRow::Written(boxed.as_bytes())]);
    } else {
        print(req, &format!("PHP Version => {PHP_VERSION}\n"));
    }
    let uname = String::from_utf8_lossy(&get_uname(b'a')).into_owned();
    let api = pretty_name(req.environment().sapi_name()).to_owned();
    let defs = req.environment().build_defs().clone();
    let [ini_path, loaded, scan_dir, scanned] = ini_cells(req);
    let wrappers: Vec<Vec<u8>> = req
        .stream_wrappers()
        .into_iter()
        .map(String::into_bytes)
        .collect();
    let transports: Vec<Vec<u8>> = req
        .transports()
        .into_iter()
        .map(|t| t.as_bytes().to_vec())
        .collect();
    let filters = req.stream_filters();
    let multibyte = req
        .extensions()
        .multibyte_provider()
        .and_then(|provider| req.extensions().name(provider))
        .map_or_else(
            || String::from("disabled"),
            |name| format!("provided by {name}"),
        );
    let streams = [
        stream_hash(req, "PHP Streams", &wrappers),
        stream_hash(req, "Stream Socket Transports", &transports),
        stream_hash(req, "Stream Filters", &filters),
    ];
    let mut cells: Vec<[&str; 2]> = vec![
        ["System", &uname],
        ["Build Date", defs.date()],
        ["Build System", defs.system()],
    ];
    if let Some(provider) = defs.provider() {
        cells.push(["Build Provider", provider]);
    }
    cells.extend([
        ["Configure Command", defs.configure()],
        ["Server API", &api],
        ["Virtual Directory Support", "disabled"],
        ["Configuration File (php.ini) Path", &ini_path],
        ["Loaded Configuration File", &loaded],
        ["Scan this dir for additional .ini files", &scan_dir],
        ["Additional .ini files parsed", &scanned],
        ["PHP API", "20250925"],
        ["PHP Extension", "20250925"],
        ["Zend Extension", "420250925"],
        ["Zend Extension Build", "API420250925,NTS"],
        ["PHP Extension Build", "API20250925,NTS"],
        ["PHP Integer Size", "64 bits"],
        ["Debug Build", "no"],
        ["Thread Safety", "disabled"],
        ["Zend Signal Handling", "disabled"],
        ["Zend Memory Manager", "enabled"],
        ["Zend Multibyte Support", &multibyte],
        ["Zend Max Execution Timers", "disabled"],
        ["IPv6 Support", "enabled"],
        ["DTrace Support", "disabled"],
    ]);
    let mut rows: Vec<InfoRow<'_>> = cells.iter().map(|cells| InfoRow::Row(cells)).collect();
    rows.extend(streams.iter().map(|written| InfoRow::Written(written)));
    req.extensions_mut().info_rows(&rows);
    let zend = zend_version(req);
    let mut contents = Vec::new();
    if html(req) {
        contents.extend_from_slice(
            format!(
                "<a href=\"https://www.zend.com/\"><img src=\"{}\" alt=\"Zend logo\" /></a>\n",
                logos::ZEND_LOGO
            )
            .as_bytes(),
        );
    }
    contents.extend_from_slice(b"This program makes use of the Zend Scripting Language Engine:");
    contents.extend_from_slice(if html(req) { b"<br />" } else { b"\n" });
    // `zend_html_puts` in HTML, which leaves the engine's own lines as
    // they are: they hold no `<`, `>`, `&` or tab.
    contents.extend_from_slice(zend.as_bytes());
    plain_box(req, &contents);
}

/// The modules, sorted by `module_name_cmp` (`strcasecmp`), each as
/// `php_info_print_module` prints it, then `Additional Modules`, which
/// lists none since every bundled extension has a version.
fn modules(req: &mut Request) {
    let mut names = req.extensions().loaded_extensions();
    names.sort_by_key(|n| n.to_ascii_lowercase());
    for name in names {
        if let Some(id) = req.extensions().named(name.as_bytes()) {
            req.extensions_mut().info(id);
        }
    }
    section(req, "Additional Modules");
    req.extensions_mut()
        .info_rows(&[InfoRow::Header(&["Module Name"])]);
}

/// `php_print_gpcse_array`'s rows (`Z/ext/standard/info.c:170`):
/// `$name['key']` and the value, an array as `print_r` renders it, in
/// `<pre>` in HTML, anything else as its string, `<i>no value</i>` for
/// an empty one in HTML. The auto-global is made first where it is armed
/// (`zend_is_auto_global`), and what is printed is the program's,
/// `EG(symbol_table)`'s, with its writes.
fn gpcse(req: &mut Request, name: &str) -> Result<Vec<u8>, Thrown> {
    let html = html(req);
    let mut out = Vec::new();
    let _ = req.variables_mut().superglobal(name.as_bytes());
    let Some(array) = req
        .variables_mut()
        .global(name.as_bytes())
        .and_then(|value| value.deref().as_array())
    else {
        return Ok(out);
    };
    // The rows are walked with no owner of the table taken: the symbol
    // table holds it, and where the program frees it under the walk, the
    // walk ends there.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        if html {
            out.extend_from_slice(b"<tr><td class=\"e\">");
        }
        out.extend_from_slice(format!("${name}['").as_bytes());
        match key {
            Key::Int(n) => out.extend_from_slice(Decimal::of(n).as_bytes()),
            Key::Str(s) if html => {
                let escaped = esc(req, s.as_bytes());
                out.extend_from_slice(&escaped);
            }
            Key::Str(s) => out.extend_from_slice(s.as_bytes()),
        }
        out.extend_from_slice(if html {
            b"']</td><td class=\"v\">"
        } else {
            b"'] => "
        });
        let value = value.deref();
        let mut shown = Vec::new();
        crate::var::render(req, &mut shown, &value, 0)?;
        match (html, value.as_array().is_some()) {
            (false, _) => out.extend_from_slice(&shown),
            (true, true) => {
                out.extend_from_slice(b"<pre>");
                let escaped = esc(req, &shown);
                out.extend_from_slice(&escaped);
                out.extend_from_slice(b"</pre>");
            }
            (true, false) if shown.is_empty() => out.extend_from_slice(b"<i>no value</i>"),
            (true, false) => {
                let escaped = esc(req, &shown);
                out.extend_from_slice(&escaped);
            }
        }
        out.extend_from_slice(if html { b"</td></tr>\n" } else { b"\n" });
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// The license, as text.
const LICENSE: &str = "\nPHP License\n\
This program is free software; you can redistribute it and/or modify\n\
it under the terms of the PHP License as published by the PHP Group\n\
and included in the distribution in the file:  LICENSE\n\
\n\
This program is distributed in the hope that it will be useful,\n\
but WITHOUT ANY WARRANTY; without even the implied warranty of\n\
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.\n\
\n\
If you did not receive a copy of the PHP license, or have any\n\
questions about PHP licensing, please contact license@php.net.\n";

/// The license, as the HTML page's box holds it.
const LICENSE_HTML: &str = "<p>\n\
This program is free software; you can redistribute it and/or modify it under the terms of the PHP License as published by the PHP Group and included in the distribution in the file:  LICENSE\n\
</p>\n\
<p>This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.\n\
</p>\n\
<p>If you did not receive a copy of the PHP license, or have any questions about PHP licensing, please contact license@php.net.\n\
</p>\n";

/// `php_print_info_htmlhead` (`Z/ext/standard/info.c:704`): the doctype,
/// the `<head>` with `php_info_print_style`'s CSS and the title, and the
/// centred body opened.
fn html_head(req: &mut Request) {
    print(
        req,
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \"DTD/xhtml1-transitional.dtd\">\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head>\n<style type=\"text/css\">\n",
    );
    print(req, CSS);
    print(
        req,
        &format!(
            "</style>\n<title>PHP {PHP_VERSION} - phpinfo()</title><meta name=\"ROBOTS\" content=\"NOINDEX,NOFOLLOW,NOARCHIVE\" /></head>\n<body><div class=\"center\">\n"
        ),
    );
}

/// `php_info_print_css` (`Z/ext/standard/css.c:20`).
const CSS: &str = "body {background-color: #fff; color: #222; font-family: sans-serif;}\n\
pre {margin: 0; font-family: monospace;}\n\
a {color: inherit;}\n\
a:hover {text-decoration: none;}\n\
table {border-collapse: collapse; border: 0; width: 934px; box-shadow: 1px 2px 3px rgba(0, 0, 0, 0.2);}\n\
.center {text-align: center;}\n\
.center table {margin: 1em auto; text-align: left;}\n\
.center th {text-align: center !important;}\n\
td, th {border: 1px solid #666; font-size: 75%; vertical-align: baseline; padding: 4px 5px;}\n\
th {position: sticky; top: 0; background: inherit;}\n\
h1 {font-size: 150%;}\n\
h2 {font-size: 125%;}\n\
h2 > a {text-decoration: none;}\n\
h2 > a:hover {text-decoration: underline;}\n\
.p {text-align: left;}\n\
.e {background-color: #ccf; width: 300px; font-weight: bold;}\n\
.h {background-color: #99c; font-weight: bold;}\n\
.v {background-color: #ddd; max-width: 300px; overflow-x: auto; word-wrap: break-word;}\n\
.v i {color: #999;}\n\
img {float: right; border: 0;}\n\
hr {width: 934px; background-color: #ccc; border: 0; height: 1px;}\n\
:root {--php-dark-grey: #333; --php-dark-blue: #4F5B93; --php-medium-blue: #8892BF; --php-light-blue: #E2E4EF; --php-accent-purple: #793862}\
@media (prefers-color-scheme: dark) {\n\
\x20 body {background: var(--php-dark-grey); color: var(--php-light-blue)}\n\
\x20 .h td, td.e, th {border-color: #606A90}\n\
\x20 td {border-color: #505153}\n\
\x20 .e {background-color: #404A77}\n\
\x20 .h {background-color: var(--php-dark-blue)}\n\
\x20 .v {background-color: var(--php-dark-grey)}\n\
\x20 hr {background-color: #505153}\n\
}\n";

/// `php_print_info(flag)` (`Z/ext/standard/info.c:754`), in the form the
/// host prints.
fn print_info(req: &mut Request, flag: i64) -> Result<(), Thrown> {
    if html(req) {
        html_head(req);
    } else {
        print(req, "phpinfo()\n");
    }
    if flag & INFO_GENERAL != 0 {
        general(req);
    }
    req.ini_mut().sort_entries();
    if flag & INFO_CONFIGURATION != 0 {
        hr(req);
        if html(req) {
            print(req, "<h1>Configuration</h1>\n");
        } else {
            section(req, "Configuration");
        }
        if flag & INFO_MODULES == 0 {
            section(req, "PHP Core");
            req.ini_mut().core_ini_info();
        }
    }
    if flag & INFO_MODULES != 0 {
        modules(req);
    }
    if flag & INFO_ENVIRONMENT != 0 {
        section(req, "Environment");
        let pairs: Vec<(String, String)> = req
            .environment()
            .env_pairs()
            .iter()
            .map(|(name, value)| {
                (
                    String::from_utf8_lossy(name).into_owned(),
                    String::from_utf8_lossy(value).into_owned(),
                )
            })
            .collect();
        let cells: Vec<[&str; 2]> = pairs
            .iter()
            .map(|(n, v)| [n.as_str(), v.as_str()])
            .collect();
        let mut rows = vec![InfoRow::Header(&["Variable", "Value"])];
        rows.extend(cells.iter().map(|cells| InfoRow::Row(cells)));
        req.extensions_mut().info_rows(&rows);
    }
    if flag & INFO_VARIABLES != 0 {
        section(req, "PHP Variables");
        let mut own: Vec<[String; 2]> = Vec::new();
        for name in ["PHP_SELF", "PHP_AUTH_TYPE", "PHP_AUTH_USER", "PHP_AUTH_PW"] {
            if let Some(value) = req.variables_mut().global(name.as_bytes())
                && let Some(value) = value.as_str_bytes()
            {
                own.push([name.to_owned(), String::from_utf8_lossy(value).into_owned()]);
            }
        }
        let mut written = Vec::new();
        for name in [
            "_REQUEST", "_GET", "_POST", "_FILES", "_COOKIE", "_SERVER", "_ENV",
        ] {
            written.extend(gpcse(req, name)?);
        }
        let cells: Vec<[&str; 2]> = own.iter().map(|[n, v]| [n.as_str(), v.as_str()]).collect();
        let mut rows = vec![InfoRow::Header(&["Variable", "Value"])];
        rows.extend(cells.iter().map(|cells| InfoRow::Row(cells)));
        rows.push(InfoRow::Written(&written));
        req.extensions_mut().info_rows(&rows);
    }
    if flag & INFO_CREDITS != 0 {
        hr(req);
        print_credits(req, CREDITS_ALL & !CREDITS_FULLPAGE);
    }
    if flag & INFO_LICENSE != 0 {
        if html(req) {
            section(req, "PHP License");
            plain_box(req, LICENSE_HTML.as_bytes());
        } else {
            print(req, LICENSE);
        }
    }
    if html(req) {
        print(req, "</div></body></html>");
    }
    Ok(())
}

/// `phpinfo(int $flags = INFO_ALL): true`, `PHP_FUNCTION(phpinfo)`
/// (`Z/ext/standard/info.c:1252`): the sections the flags name.
///
/// ```php
/// phpinfo(INFO_LICENSE);
/// ob_start(); phpinfo(INFO_GENERAL); $g = ob_get_clean();
/// var_dump(str_starts_with($g, "phpinfo()\nPHP Version => " . PHP_VERSION . "\n\nSystem => " . php_uname() . "\n"));
/// ob_start(); var_dump(phpinfo(INFO_MODULES | INFO_CONFIGURATION)); $m = ob_get_clean();
/// var_dump(str_contains($m, "\n\n _____"), str_contains($m, "\nstandard\n"), str_contains($m, "\nCore\n\nPHP Version => "), str_ends_with($m, "\nAdditional Modules\n\nModule Name\nbool(true)\n"));
/// ```
/// ```expect
/// phpinfo()
///
/// PHP License
/// This program is free software; you can redistribute it and/or modify
/// it under the terms of the PHP License as published by the PHP Group
/// and included in the distribution in the file:  LICENSE
///
/// This program is distributed in the hope that it will be useful,
/// but WITHOUT ANY WARRANTY; without even the implied warranty of
/// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
///
/// If you did not receive a copy of the PHP license, or have any
/// questions about PHP licensing, please contact license@php.net.
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// Whatever the flags, the ini table is sorted by name before anything
/// past the general block is printed, so each extension's directives are
/// listed sorted, and so is every walk of the table after the call
/// ([`Ini::sort_entries`]):
///
/// ```php
/// ob_start(); phpinfo(INFO_MODULES); preg_match_all('/^session\.\w+/m', ob_get_clean(), $m);
/// echo implode(' ', array_slice($m[0], 0, 4)), "\n";
/// ```
/// ```expect
/// session.auto_start session.cache_expire session.cache_limiter session.cookie_domain
/// ```
///
/// ```php
/// $iconv = fn () => implode(' ', array_keys((new ReflectionExtension('iconv'))->getINIEntries())) . "\n";
/// echo $iconv(); phpinfo(0); echo $iconv();
/// ```
/// ```expect
/// iconv.input_encoding iconv.output_encoding iconv.internal_encoding
/// phpinfo()
/// iconv.input_encoding iconv.internal_encoding iconv.output_encoding
/// ```
///
/// A host serving the web prints `info.c`'s HTML page: the head with its
/// style, the sections, and the body closed after the last:
///
/// ```php
/// phpinfo(INFO_LICENSE);
/// ob_start(); phpinfo(INFO_MODULES | INFO_GENERAL); $m = ob_get_clean();
/// var_dump(str_contains($m, "<h2><a name=\"module_core\" href=\"#module_core\">Core</a></h2>\n<table>\n<tr><td class=\"e\">PHP Version </td><td class=\"v\">8.5.9 </td></tr>\n</table>\n<table>\n<tr class=\"h\"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>\n"), str_contains($m, "<h2>Additional Modules</h2>\n<table>\n<tr class=\"h\"><th>Module Name</th></tr>\n</table>\n"), str_ends_with($m, "</div></body></html>"));
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// <!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Transitional//EN" "DTD/xhtml1-transitional.dtd">
/// <html xmlns="http://www.w3.org/1999/xhtml"><head>
/// <style type="text/css">
/// body {background-color: #fff; color: #222; font-family: sans-serif;}
/// pre {margin: 0; font-family: monospace;}
/// a {color: inherit;}
/// a:hover {text-decoration: none;}
/// table {border-collapse: collapse; border: 0; width: 934px; box-shadow: 1px 2px 3px rgba(0, 0, 0, 0.2);}
/// .center {text-align: center;}
/// .center table {margin: 1em auto; text-align: left;}
/// .center th {text-align: center !important;}
/// td, th {border: 1px solid #666; font-size: 75%; vertical-align: baseline; padding: 4px 5px;}
/// th {position: sticky; top: 0; background: inherit;}
/// h1 {font-size: 150%;}
/// h2 {font-size: 125%;}
/// h2 > a {text-decoration: none;}
/// h2 > a:hover {text-decoration: underline;}
/// .p {text-align: left;}
/// .e {background-color: #ccf; width: 300px; font-weight: bold;}
/// .h {background-color: #99c; font-weight: bold;}
/// .v {background-color: #ddd; max-width: 300px; overflow-x: auto; word-wrap: break-word;}
/// .v i {color: #999;}
/// img {float: right; border: 0;}
/// hr {width: 934px; background-color: #ccc; border: 0; height: 1px;}
/// :root {--php-dark-grey: #333; --php-dark-blue: #4F5B93; --php-medium-blue: #8892BF; --php-light-blue: #E2E4EF; --php-accent-purple: #793862}@media (prefers-color-scheme: dark) {
///   body {background: var(--php-dark-grey); color: var(--php-light-blue)}
///   .h td, td.e, th {border-color: #606A90}
///   td {border-color: #505153}
///   .e {background-color: #404A77}
///   .h {background-color: var(--php-dark-blue)}
///   .v {background-color: var(--php-dark-grey)}
///   hr {background-color: #505153}
/// }
/// </style>
/// <title>PHP 8.5.12 - phpinfo()</title><meta name="ROBOTS" content="NOINDEX,NOFOLLOW,NOARCHIVE" /></head>
/// <body><div class="center">
/// <h2>PHP License</h2>
/// <table>
/// <tr class="v"><td>
/// <p>
/// This program is free software; you can redistribute it and/or modify it under the terms of the PHP License as published by the PHP Group and included in the distribution in the file:  LICENSE
/// </p>
/// <p>This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
/// </p>
/// <p>If you did not receive a copy of the PHP license, or have any questions about PHP licensing, please contact license@php.net.
/// </p>
/// </td></tr>
/// </table>
/// </div></body></html>bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// The declarations of `info.c`.
///
/// ```php
/// foreach (['phpinfo', 'phpversion', 'phpcredits', 'php_sapi_name', 'php_uname', 'php_ini_scanned_files', 'php_ini_loaded_file'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function phpinfo ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> int $flags = INFO_ALL ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function phpversion ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> ?string $extension = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function phpcredits ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> int $flags = CREDITS_ALL ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function php_sapi_name ] {
///
///   - Parameters [0] {
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function php_uname ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> string $mode = "a" ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function php_ini_scanned_files ] {
///
///   - Parameters [0] {
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function php_ini_loaded_file ] {
///
///   - Parameters [0] {
///   }
///   - Return [ string|false ]
/// }
/// ```
#[php_function]
fn phpinfo(req: &mut Request, #[php(default = INFO_ALL)] flags: i64) -> Result<True, Thrown> {
    let flag = flags;
    print_info(req, i64::from(int(flag)))?;
    Ok(True)
}

/// The low 32 bits of a `zend_long`, as the `(int)flag` cast keeps them.
fn int(n: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let n = n as i32;
    n
}

/// `phpversion(?string $extension = null): string|false`,
/// `PHP_FUNCTION(phpversion)` (`Z/ext/standard/info.c:1270`): php's
/// version, or the extension's, `zend_get_module_version` over the
/// lowered name.
///
/// ```php
/// var_dump(phpversion() === PHP_VERSION, phpversion('standard') === PHP_VERSION, phpversion('Core') === PHP_VERSION, phpversion('nope'), phpversion(null) === PHP_VERSION);
/// var_dump(phpversion('DOM'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// string(8) "20031129"
/// ```
#[php_function]
fn phpversion(
    req: &mut Request,
    #[php(default = null)] extension: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(name) = extension else {
        return PhpString::new(req, PHP_VERSION.as_bytes()).map(OrFalse::Value);
    };
    let Some(extension) = req.extensions().named(name.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    let version = req.extensions().version(extension).unwrap_or_default();
    PhpString::new(req, version.as_bytes()).map(OrFalse::Value)
}

/// `php_print_credits(flag)` (`Z/ext/standard/credits.c:26`), in the
/// form the host prints, the HTML page's head and foot around it where
/// `CREDITS_FULLPAGE` asks.
fn print_credits(req: &mut Request, flag: i64) {
    let html = html(req);
    let full = html && flag & CREDITS_FULLPAGE != 0;
    if full {
        html_head(req);
    }
    print(
        req,
        if html {
            "<h1>PHP Credits</h1>\n"
        } else {
            "PHP Credits\n"
        },
    );
    let lines = |lines: &'static [(&'static str, &'static str)]| -> Vec<[&'static str; 2]> {
        lines.iter().map(|(what, who)| [*what, *who]).collect()
    };
    let credit_table =
        |req: &mut Request, colspan: &str, header: Option<[&str; 2]>, body: &[[&str; 2]]| {
            let mut rows = vec![InfoRow::Colspan(2, colspan)];
            if let Some(header) = &header {
                rows.push(InfoRow::Header(header));
            }
            rows.extend(body.iter().map(|cells| InfoRow::Row(cells)));
            req.extensions_mut().info_rows(&rows);
        };
    if flag & CREDITS_GROUP != 0 {
        req.extensions_mut()
            .info_rows(&[InfoRow::Header(&["PHP Group"]), InfoRow::Row(&[GROUP])]);
    }
    if flag & CREDITS_GENERAL != 0 {
        let design = if html {
            "Language Design &amp; Concept"
        } else {
            "Language Design & Concept"
        };
        req.extensions_mut()
            .info_rows(&[InfoRow::Header(&[design]), InfoRow::Row(&[DESIGN])]);
        credit_table(
            req,
            "PHP Authors",
            Some(["Contribution", "Authors"]),
            &lines(&AUTHORS),
        );
    }
    if flag & CREDITS_SAPI != 0 {
        credit_table(
            req,
            "SAPI Modules",
            Some(["Contribution", "Authors"]),
            &lines(&SAPI_CREDITS),
        );
    }
    if flag & CREDITS_MODULES != 0 {
        credit_table(
            req,
            "Module Authors",
            Some(["Module", "Authors"]),
            &lines(&MODULE_CREDITS),
        );
    }
    if flag & CREDITS_DOCS != 0 {
        credit_table(req, "PHP Documentation", None, &lines(&DOCS));
    }
    if flag & CREDITS_QA != 0 {
        req.extensions_mut().info_rows(&[
            InfoRow::Header(&["PHP Quality Assurance Team"]),
            InfoRow::Row(&[QA]),
        ]);
    }
    if flag & CREDITS_WEB != 0 {
        credit_table(req, "Websites and Infrastructure team", None, &lines(&WEB));
    }
    if full {
        print(req, "</div></body></html>\n");
    }
}

/// `phpcredits(int $flags = CREDITS_ALL): true`,
/// `PHP_FUNCTION(phpcredits)` (`Z/ext/standard/info.c:1293`).
///
/// ```php
/// phpcredits(CREDITS_GROUP | CREDITS_QA);
/// ob_start(); var_dump(phpcredits()); $all = ob_get_clean();
/// var_dump(str_contains($all, "\n" . str_repeat(' ', 28) . "PHP Documentation" . str_repeat(' ', 28) . "\nAuthors => Mehdi Achour"));
/// var_dump(str_contains($all, "CLI => Edin Kadribasic"), str_contains($all, "Zlib => Rasmus Lerdorf"), str_ends_with($all, "Alex Schoenmaker\nbool(true)\n"));
/// ```
/// ```expect
/// PHP Credits
///
/// PHP Group
/// Thies C. Arntzen, Stig Bakken, Shane Caraveo, Andi Gutmans, Rasmus Lerdorf, Sam Ruby, Sascha Schumann, Zeev Suraski, Jim Winstead, Andrei Zmievski
///
/// PHP Quality Assurance Team
/// Ilia Alshanetsky, Joerg Behrens, Antony Dovgal, Stefan Esser, Moriyoshi Koizumi, Magnus Maatta, Sebastian Nohn, Derick Rethans, Melvyn Sopacua, Pierre-Alain Joye, Dmitry Stogov, Felipe Pena, David Soria Parra, Stanislav Malyshev, Julien Pauli, Stephen Zarkos, Anatol Belski, Remi Collet, Ferenc Kovacs
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
///
/// A host serving the web prints `credits.c`'s HTML, the headers as they
/// are and each row's cells escaped:
///
/// ```php
/// phpcredits(CREDITS_GROUP | CREDITS_DOCS);
/// ```
/// ```request
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// <h1>PHP Credits</h1>
/// <table>
/// <tr class="h"><th>PHP Group</th></tr>
/// <tr><td class="e">Thies C. Arntzen, Stig Bakken, Shane Caraveo, Andi Gutmans, Rasmus Lerdorf, Sam Ruby, Sascha Schumann, Zeev Suraski, Jim Winstead, Andrei Zmievski </td></tr>
/// </table>
/// <table>
/// <tr class="h"><th colspan="2">PHP Documentation</th></tr>
/// <tr><td class="e">Authors </td><td class="v">Mehdi Achour, Friedhelm Betz, Antony Dovgal, Nuno Lopes, Hannes Magnusson, Philip Olson, Georg Richter, Damien Seguy, Jakub Vrana, Adam Harvey </td></tr>
/// <tr><td class="e">Editor </td><td class="v">Peter Cowburn </td></tr>
/// <tr><td class="e">User Note Maintainers </td><td class="v">Daniel P. Brown, Thiago Henrique Pojda </td></tr>
/// <tr><td class="e">Other Contributors </td><td class="v">Previously active authors, editors and other contributors are listed in the manual. </td></tr>
/// </table>
/// ```
#[php_function]
fn phpcredits(req: &mut Request, #[php(default = CREDITS_ALL)] flags: i64) -> True {
    let flag = flags;
    print_credits(req, i64::from(int(flag)));
    True
}

/// `php_sapi_name(): string|false`, `PHP_FUNCTION(php_sapi_name)`
/// (`Z/ext/standard/info.c:1307`): `sapi_module.name`.
///
/// ```php
/// var_dump(php_sapi_name());
/// ```
/// ```expect
/// string(3) "cli"
/// ```
#[php_function]
fn php_sapi_name(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = req.environment().sapi_name().to_owned();
    PhpString::new(req, name.as_bytes()).map(OrFalse::Value)
}

/// `php_uname(string $mode = "a"): string`, `PHP_FUNCTION(php_uname)`
/// (`Z/ext/standard/info.c:1316`): one field of `uname(2)` or all
/// five. What they hold is the host's, so the witness pins how they
/// join and the checks.
///
/// ```php
/// var_dump(php_uname() === php_uname('s') . ' ' . php_uname('n') . ' ' . php_uname('r') . ' ' . php_uname('v') . ' ' . php_uname('m'), php_uname('s'));
/// foreach (['', 'ab', 'x'] as $m) { try { php_uname($m); } catch (ValueError $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// bool(true)
/// string(6) "Darwin"
/// php_uname(): Argument #1 ($mode) must be a single character
/// php_uname(): Argument #1 ($mode) must be a single character
/// php_uname(): Argument #1 ($mode) must be one of "a", "m", "n", "r", "s", or "v"
/// ```
#[php_function]
fn php_uname(
    req: &mut Request,
    #[php(default = "a")] mode: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let [mode] = mode.as_bytes() else {
        return Err(arg_value_error(req, 1, "be a single character"));
    };
    let mode = *mode;
    if !b"amnrsv".contains(&mode) {
        return Err(arg_value_error(
            req,
            1,
            "be one of \"a\", \"m\", \"n\", \"r\", \"s\", or \"v\"",
        ));
    }
    PhpString::new(req, &get_uname(mode))
}

/// `php_ini_scanned_files`, the string `php_init_config` joins: each
/// scanned file that parsed, `,\n` between and `\n` after the last;
/// none where none was.
fn scanned_list(req: &Request) -> Option<Vec<u8>> {
    let files: Vec<&[u8]> = req.ini().files().scanned().collect();
    (!files.is_empty()).then(|| [files.join(b",\n".as_slice()), b"\n".to_vec()].concat())
}

/// The general table's four php.ini rows: `PHP_CONFIG_FILE_PATH`, then
/// the file read, the scan path and the files scanned, `(none)` for each
/// there was none of.
fn ini_cells(req: &Request) -> [String; 4] {
    let files = req.ini().files();
    let scanned = scanned_list(req);
    [
        Some(files.ini_path()),
        files.loaded(),
        files.scan_dir(),
        scanned.as_deref(),
    ]
    .map(|path| {
        path.map_or_else(
            || String::from("(none)"),
            |path| String::from_utf8_lossy(path).into_owned(),
        )
    })
}

/// `php_ini_scanned_files(): string|false`,
/// `PHP_FUNCTION(php_ini_scanned_files)` (`Z/ext/standard/info.c:1336`):
/// the files the scan read, `false` where it read none, as under `-n`
/// or an empty `PHP_INI_SCAN_DIR`. A file that does not parse is not
/// listed, and a scanned value stands over the php.ini's and under a
/// `-d`:
///
/// ```php
/// var_dump(php_ini_scanned_files());
/// mkdir('d'); file_put_contents('d/b.ini', "precision=5\n"); file_put_contents('d/a.ini', "precision=4\nserialize_precision=4\n"); file_put_contents('d/c.ini', "x=(\n"); file_put_contents('d/x.txt', "precision=9\n");
/// file_put_contents('x.ini', "precision=3\nserialize_precision=3\nmemory_limit=3M\n");
/// $p = proc_open([PHP_BINARY, '-c', 'x.ini', '-d', 'serialize_precision=6', '-r', 'echo ini_get("precision"), " ", ini_get("serialize_precision"), " ", ini_get("memory_limit"), "\n", str_replace(getcwd(), ".", php_ini_scanned_files());'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, ['PHP_INI_SCAN_DIR' => getcwd() . '/d']);
/// echo stream_get_contents($pipes[1]), str_replace(getcwd(), '.', stream_get_contents($pipes[2])); proc_close($p);
/// ```
/// ```expect process
/// bool(false)
/// 5 6 3M
/// ./d/a.ini,
/// ./d/b.ini
/// PHP:  syntax error, unexpected END_OF_LINE in ./d/c.ini on line 2
/// ```
#[php_function]
fn php_ini_scanned_files(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match scanned_list(req) {
        Some(list) => PhpString::new(req, &list).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `php_ini_loaded_file(): string|false`,
/// `PHP_FUNCTION(php_ini_loaded_file)` (`Z/ext/standard/info.c:1349`):
/// the php.ini read, as its path was recorded, `false` where none was;
/// under `-n` a `-c` is still read, though nothing is scanned:
///
/// ```php
/// var_dump(php_ini_loaded_file());
/// file_put_contents('n.ini', "precision=3\n");
/// $p = proc_open([PHP_BINARY, '-n', '-c', 'n.ini', '-r', 'var_dump(basename(php_ini_loaded_file()), php_ini_scanned_files(), ini_get("precision"), get_cfg_var("cfg_file_path") === php_ini_loaded_file());'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes, null, ['PHP_INI_SCAN_DIR' => getcwd()]);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]); proc_close($p);
/// ```
/// ```expect process
/// bool(false)
/// string(5) "n.ini"
/// bool(false)
/// string(1) "3"
/// bool(true)
/// ```
#[php_function]
fn php_ini_loaded_file(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match req.ini().files().loaded().map(<[u8]>::to_vec) {
        Some(path) => PhpString::new(req, &path).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// The PHP Group row, `Z/ext/standard/credits.c:41`.
const GROUP: &str = "Thies C. Arntzen, Stig Bakken, Shane Caraveo, Andi Gutmans, Rasmus Lerdorf, Sam Ruby, Sascha Schumann, Zeev Suraski, Jim Winstead, Andrei Zmievski";

/// The Language Design & Concept row, `Z/ext/standard/credits.c:52`.
const DESIGN: &str = "Andi Gutmans, Rasmus Lerdorf, Zeev Suraski, Marcus Boerger";

/// The PHP Authors lines, `Z/ext/standard/credits.c:59`.
const AUTHORS: [(&str, &str); 9] = [
    (
        "Zend Scripting Language Engine",
        "Andi Gutmans, Zeev Suraski, Stanislav Malyshev, Marcus Boerger, Dmitry Stogov, Xinchen Hui, Nikita Popov",
    ),
    (
        "Extension Module API",
        "Andi Gutmans, Zeev Suraski, Andrei Zmievski",
    ),
    (
        "UNIX Build and Modularization",
        "Stig Bakken, Sascha Schumann, Jani Taskinen, Peter Kokot",
    ),
    (
        "Windows Support",
        "Shane Caraveo, Zeev Suraski, Wez Furlong, Pierre-Alain Joye, Anatol Belski, Kalle Sommer Nielsen",
    ),
    (
        "Server API (SAPI) Abstraction Layer",
        "Andi Gutmans, Shane Caraveo, Zeev Suraski",
    ),
    ("Streams Abstraction Layer", "Wez Furlong, Sara Golemon"),
    (
        "PHP Data Objects Layer",
        "Wez Furlong, Marcus Boerger, Sterling Hughes, George Schlossnagle, Ilia Alshanetsky",
    ),
    (
        "Output Handler",
        "Zeev Suraski, Thies C. Arntzen, Marcus Boerger, Michael Wallner",
    ),
    (
        "Consistent 64 bit support",
        "Anthony Ferrara, Anatol Belski",
    ),
];

/// `credits_sapi.h`.
const SAPI_CREDITS: [(&str, &str); 7] = [
    (
        "Apache 2 Handler",
        "Ian Holsman, Justin Erenkrantz (based on Apache 2 Filter code)",
    ),
    (
        "CGI / FastCGI",
        "Rasmus Lerdorf, Stig Bakken, Shane Caraveo, Dmitry Stogov",
    ),
    (
        "CLI",
        "Edin Kadribasic, Marcus Boerger, Johannes Schlueter, Moriyoshi Koizumi, Xinchen Hui",
    ),
    ("Embed", "Edin Kadribasic"),
    (
        "FastCGI Process Manager",
        "Andrei Nigmatulin, dreamcat4, Antony Dovgal, Jerome Loyet",
    ),
    ("litespeed", "George Wang"),
    ("phpdbg", "Felipe Pena, Joe Watkins, Bob Weinand"),
];

/// The PHP Documentation lines, `Z/ext/standard/credits.c:92`.
const DOCS: [(&str, &str); 4] = [
    (
        "Authors",
        "Mehdi Achour, Friedhelm Betz, Antony Dovgal, Nuno Lopes, Hannes Magnusson, Philip Olson, Georg Richter, Damien Seguy, Jakub Vrana, Adam Harvey",
    ),
    ("Editor", "Peter Cowburn"),
    (
        "User Note Maintainers",
        "Daniel P. Brown, Thiago Henrique Pojda",
    ),
    (
        "Other Contributors",
        "Previously active authors, editors and other contributors are listed in the manual.",
    ),
];

/// The QA row, `Z/ext/standard/credits.c:103`.
const QA: &str = "Ilia Alshanetsky, Joerg Behrens, Antony Dovgal, Stefan Esser, Moriyoshi Koizumi, Magnus Maatta, Sebastian Nohn, Derick Rethans, Melvyn Sopacua, Pierre-Alain Joye, Dmitry Stogov, Felipe Pena, David Soria Parra, Stanislav Malyshev, Julien Pauli, Stephen Zarkos, Anatol Belski, Remi Collet, Ferenc Kovacs";

/// The Websites and Infrastructure lines, `Z/ext/standard/credits.c:112`.
const WEB: [(&str, &str); 4] = [
    (
        "PHP Websites Team",
        "Rasmus Lerdorf, Hannes Magnusson, Philip Olson, Lukas Kahwe Smith, Pierre-Alain Joye, Kalle Sommer Nielsen, Peter Cowburn, Adam Harvey, Ferenc Kovacs, Levi Morrison",
    ),
    ("Event Maintainers", "Damien Seguy, Daniel P. Brown"),
    ("Network Infrastructure", "Daniel P. Brown"),
    ("Windows Infrastructure", "Alex Schoenmaker"),
];

/// `credits_ext.h`, which `scripts/dev/credits` renders from each
/// extension's `CREDITS` file: the extension and its authors.
const MODULE_CREDITS: [(&str, &str); 66] = [
    ("BC Math", "Andi Gutmans"),
    ("Bzip2", "Sterling Hughes"),
    (
        "Calendar",
        "Shane Caraveo, Colin Viebrock, Hartmut Holzgraefe, Wez Furlong",
    ),
    ("COM and .Net", "Wez Furlong"),
    ("ctype", "Hartmut Holzgraefe"),
    ("cURL", "Sterling Hughes"),
    ("Date/Time Support", "Derick Rethans"),
    (
        "DB-LIB (MS SQL, Sybase)",
        "Wez Furlong, Frank M. Kromann, Adam Baratz",
    ),
    ("DBA", "Sascha Schumann, Marcus Boerger"),
    (
        "DOM",
        "Christian Stocker, Rob Richards, Marcus Boerger, Nora Dossche",
    ),
    ("enchant", "Pierre-Alain Joye, Ilia Alshanetsky"),
    ("EXIF", "Rasmus Lerdorf, Marcus Boerger"),
    ("FFI", "Dmitry Stogov"),
    (
        "fileinfo",
        "Ilia Alshanetsky, Pierre Alain Joye, Scott MacVicar, Derick Rethans, Anatol Belski",
    ),
    ("Firebird driver for PDO", "Ard Biesheuvel"),
    ("FTP", "Stefan Esser, Andrew Skalski"),
    (
        "GD imaging",
        "Rasmus Lerdorf, Stig Bakken, Jim Winstead, Jouni Ahto, Ilia Alshanetsky, Pierre-Alain Joye, Marcus Boerger, Mark Randall",
    ),
    ("GetText", "Alex Plotnick"),
    ("GNU GMP support", "Stanislav Malyshev"),
    ("Iconv", "Rui Hirokawa, Stig Bakken, Moriyoshi Koizumi"),
    (
        "Input Filter",
        "Rasmus Lerdorf, Derick Rethans, Pierre-Alain Joye, Ilia Alshanetsky",
    ),
    (
        "Internationalization",
        "Ed Batutis, Vladimir Iordanov, Dmitry Lakhtyuk, Stanislav Malyshev, Vadim Savchuk, Kirti Velankar",
    ),
    ("JSON", "Jakub Zelenka, Omar Kilani, Scott MacVicar"),
    (
        "LDAP",
        "Amitay Isaacs, Eric Warnke, Rasmus Lerdorf, Gerrit Thomson, Stig Venaas",
    ),
    (
        "LIBXML",
        "Christian Stocker, Rob Richards, Marcus Boerger, Wez Furlong, Shane Caraveo",
    ),
    ("Multibyte String Functions", "Tsukada Takuya, Rui Hirokawa"),
    (
        "MySQL driver for PDO",
        "George Schlossnagle, Wez Furlong, Ilia Alshanetsky, Johannes Schlueter",
    ),
    (
        "MySQLi",
        "Zak Greant, Georg Richter, Andrey Hristov, Ulf Wendel",
    ),
    (
        "MySQLnd",
        "Andrey Hristov, Ulf Wendel, Georg Richter, Johannes Schlüter",
    ),
    ("ODBC driver for PDO", "Wez Furlong"),
    (
        "ODBC",
        "Stig Bakken, Andreas Karajannis, Frank M. Kromann, Daniel R. Kalowsky",
    ),
    (
        "Opcache",
        "Andi Gutmans, Zeev Suraski, Stanislav Malyshev, Dmitry Stogov, Xinchen Hui",
    ),
    (
        "OpenSSL",
        "Stig Venaas, Wez Furlong, Sascha Kettler, Scott MacVicar, Eliot Lear",
    ),
    ("pcntl", "Jason Greene, Arnaud Le Blanc"),
    ("Perl Compatible Regexps", "Andrei Zmievski"),
    ("PHP Archive", "Gregory Beaver, Marcus Boerger"),
    (
        "PHP Data Objects",
        "Wez Furlong, Marcus Boerger, Sterling Hughes, George Schlossnagle, Ilia Alshanetsky",
    ),
    (
        "PHP hash",
        "Sara Golemon, Rasmus Lerdorf, Stefan Esser, Michael Wallner, Scott MacVicar",
    ),
    ("Posix", "Kristian Koehntopp"),
    (
        "PostgreSQL driver for PDO",
        "Edin Kadribasic, Ilia Alshanetsky",
    ),
    (
        "PostgreSQL",
        "Jouni Ahto, Zeev Suraski, Yasuo Ohgaki, Chris Kings-Lynne",
    ),
    (
        "random",
        "Go Kudo, Tim Düsterhus, Guilliam Xavier, Christoph M. Becker, Jakub Zelenka, Bob Weinand, Máté Kocsis, and Original RNG implementators",
    ),
    ("Readline", "Thies C. Arntzen"),
    (
        "Reflection",
        "Marcus Boerger, Timm Friebe, George Schlossnagle, Andrei Zmievski, Johannes Schlueter",
    ),
    ("Sessions", "Sascha Schumann, Andrei Zmievski"),
    (
        "Shared Memory Operations",
        "Slava Poliakov, Ilia Alshanetsky",
    ),
    ("SimpleXML", "Sterling Hughes, Marcus Boerger, Rob Richards"),
    (
        "SNMP",
        "Rasmus Lerdorf, Harrie Hazewinkel, Mike Jackson, Steven Lawrance, Johann Hanne, Boris Lytochkin",
    ),
    ("SOAP", "Brad Lafountain, Shane Caraveo, Dmitry Stogov"),
    (
        "Sockets",
        "Chris Vandomelen, Sterling Hughes, Daniel Beulshausen, Jason Greene",
    ),
    ("Sodium", "Frank Denis"),
    ("SPL", "Marcus Boerger, Etienne Kneuss"),
    ("SQLite 3.x driver for PDO", "Wez Furlong"),
    ("SQLite3", "Scott MacVicar, Ilia Alshanetsky, Brad Dewar"),
    ("System V Message based IPC", "Wez Furlong"),
    ("System V Semaphores", "Tom May"),
    ("System V Shared Memory", "Christian Cartus"),
    ("tidy", "John Coggeshall, Ilia Alshanetsky"),
    ("tokenizer", "Andrei Zmievski, Johannes Schlueter"),
    (
        "uri",
        "Máté Kocsis, Tim Düsterhus, Ignace Nyamagana Butera, Arnaud Le Blanc, Dennis Snell, Nora Dossche, Nicolas Grekas",
    ),
    ("XML", "Stig Bakken, Thies C. Arntzen, Sterling Hughes"),
    ("XMLReader", "Rob Richards"),
    ("XMLWriter", "Rob Richards, Pierre-Alain Joye"),
    ("XSL", "Christian Stocker, Rob Richards"),
    ("Zip", "Pierre-Alain Joye, Remi Collet"),
    (
        "Zlib",
        "Rasmus Lerdorf, Stefan Roehrich, Zeev Suraski, Jade Nicoletti, Michael Wallner",
    ),
];

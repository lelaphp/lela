//! `Z/ext/standard/info.c` and `credits.c`: `phpinfo`, `phpversion`,
//! `phpcredits`, `php_sapi_name`, `php_uname`, `php_ini_scanned_files`
//! and `php_ini_loaded_file`, and the `INFO_*` and `CREDITS_*`
//! constants. The page is the text form `phpinfo_as_text` prints, the
//! CLI's; the general table states what this engine is: its version,
//! the host's `uname(2)`, the server API, that no ini file was read and
//! the wrappers it registered.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, True};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::Thrown;
use php_ext::request::{PHP_VERSION, Request};
use php_ext::values::{Key, PhpString};

use crate::arg_value_error;
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
    req.write(text.as_bytes());
}

/// `php_info_print_table_start` then `php_info_print_table_row` as text:
/// a blank line, then each row's cells with ` => ` between, an empty
/// cell one space.
fn table(req: &mut Request, rows: &[&[&str]]) {
    req.info_table(rows);
}

/// `php_info_print_table_row` alone, without the table's blank line.
fn row(req: &mut Request, cells: &[&str]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if cell.is_empty() {
            line.push(' ');
        } else {
            line.push_str(cell);
            if i + 1 < cells.len() {
                line.push_str(" => ");
            }
        }
    }
    line.push('\n');
    print(req, &line);
}

/// `SECTION(name)` as text: a one-column table of the name.
fn section(req: &mut Request, name: &str) {
    table(req, &[&[name]]);
}

/// `php_info_print_hr` as text.
fn hr(req: &mut Request) {
    print(
        req,
        "\n\n _______________________________________________________________________\n\n",
    );
}

/// `php_info_print_table_colspan_header` as text: the header centred in
/// 74 columns, `%*s` padding at least one space each side.
fn colspan_header(req: &mut Request, header: &str) {
    let spaces = 74_usize.saturating_sub(header.len()) / 2;
    let pad = " ".repeat(spaces.max(1));
    print(req, &format!("{pad}{header}{pad}\n"));
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

/// `php_info_print_stream_hash` as text: `\nRegistered <name> => ` and
/// the names with `, ` between and no newline after, or a
/// `none registered` row where there are none.
fn stream_hash(req: &mut Request, name: &str, names: &[String]) {
    if names.is_empty() {
        row(req, &[&format!("Registered {name}"), "none registered"]);
    } else {
        print(req, &format!("\nRegistered {name} => {}", names.join(", ")));
    }
}

/// The general table of `php_print_info` (`Z/ext/standard/info.c:766`).
fn general(req: &mut Request) {
    // The box around the version is html's alone.
    row(req, &["PHP Version", PHP_VERSION]);
    let uname = String::from_utf8_lossy(&get_uname(b'a')).into_owned();
    let api = pretty_name(req.sapi_name()).to_owned();
    table(
        req,
        &[
            &["System", &uname],
            &["Server API", &api],
            &["Virtual Directory Support", "disabled"],
            &["Configuration File (php.ini) Path", ""],
            &["Loaded Configuration File", "(none)"],
            &["Scan this dir for additional .ini files", "(none)"],
            &["Additional .ini files parsed", "(none)"],
            &["PHP API", "20250925"],
            &["PHP Extension", "20250925"],
            &["Zend Extension", "420250925"],
            &["Zend Extension Build", "API420250925,NTS"],
            &["PHP Extension Build", "API20250925,NTS"],
            &["PHP Integer Size", "64 bits"],
            &["Debug Build", "no"],
            &["Thread Safety", "disabled"],
            &["Zend Signal Handling", "disabled"],
            &["Zend Memory Manager", "enabled"],
            &["Zend Multibyte Support", "disabled"],
            &["Zend Max Execution Timers", "disabled"],
            &["IPv6 Support", "enabled"],
            &["DTrace Support", "disabled"],
        ],
    );
    let wrappers = req.stream_wrappers();
    stream_hash(req, "PHP Streams", &wrappers);
    stream_hash(req, "Stream Socket Transports", &[]);
    stream_hash(req, "Stream Filters", &[]);
    print(
        req,
        "\n\nThis program makes use of the Zend Scripting Language Engine:\n",
    );
    print(
        req,
        &format!(
            "Zend Engine v4.{}, Copyright (c) Zend Technologies\n",
            &PHP_VERSION[2..]
        ),
    );
}

/// The modules, sorted by `module_name_cmp` (`strcasecmp`), each as
/// `php_info_print_module` prints it; `Core`'s `MINFO` is the version
/// and the engine's own entries.
fn modules(req: &mut Request) {
    let mut names = req.loaded_extensions();
    if !names.iter().any(|n| n.eq_ignore_ascii_case("core")) {
        names.push(String::from("Core"));
    }
    names.sort_by_key(|n| n.to_ascii_lowercase());
    for name in names {
        if name.eq_ignore_ascii_case("core") {
            section(req, &name);
            table(req, &[&["PHP Version", PHP_VERSION]]);
            req.core_ini_info();
        } else if let Some(id) = req.extension_named(name.as_bytes()) {
            req.extension_info(id);
        }
    }
    section(req, "Additional Modules");
    table(req, &[&["Module Name"]]);
}

/// `php_print_gpcse_array` as text: `$name['key'] => value` per row,
/// an array as `print_r` renders it, anything else as its string. The
/// auto-global is made first where it is armed (`zend_is_auto_global`),
/// and what is printed is the program's, `EG(symbol_table)`'s, with its
/// writes.
fn gpcse(req: &mut Request, name: &str) -> Result<(), Thrown> {
    let _ = req.superglobal(name.as_bytes());
    let Some(array) = req
        .global(name.as_bytes())
        .and_then(|value| value.deref().as_array().copied())
    else {
        return Ok(());
    };
    for (key, value) in array.snapshot() {
        let mut line = format!("${name}['").into_bytes();
        match key {
            Key::Int(n) => line.extend_from_slice(n.to_string().as_bytes()),
            Key::Str(s) => line.extend_from_slice(s.as_bytes()),
        }
        line.extend_from_slice(b"'] => ");
        crate::var::render(req, &mut line, &value, 0)?;
        line.push(b'\n');
        req.write(&line);
    }
    Ok(())
}

/// The license box, as text.
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

/// `php_print_info(flag)` (`Z/ext/standard/info.c:754`) as text.
fn print_info(req: &mut Request, flag: i64) -> Result<(), Thrown> {
    print(req, "phpinfo()\n");
    if flag & INFO_GENERAL != 0 {
        general(req);
    }
    if flag & INFO_CONFIGURATION != 0 {
        hr(req);
        section(req, "Configuration");
        if flag & INFO_MODULES == 0 {
            section(req, "PHP Core");
            req.core_ini_info();
        }
    }
    if flag & INFO_MODULES != 0 {
        modules(req);
    }
    if flag & INFO_ENVIRONMENT != 0 {
        section(req, "Environment");
        let pairs = req.env_pairs().to_vec();
        table(req, &[&["Variable", "Value"]]);
        for (name, value) in pairs {
            let name = String::from_utf8_lossy(&name).into_owned();
            let value = String::from_utf8_lossy(&value).into_owned();
            row(req, &[&name, &value]);
        }
    }
    if flag & INFO_VARIABLES != 0 {
        section(req, "PHP Variables");
        table(req, &[&["Variable", "Value"]]);
        for name in [
            "_REQUEST", "_GET", "_POST", "_FILES", "_COOKIE", "_SERVER", "_ENV",
        ] {
            gpcse(req, name)?;
        }
    }
    if flag & INFO_CREDITS != 0 {
        hr(req);
        print_credits(req, CREDITS_ALL & !CREDITS_FULLPAGE);
    }
    if flag & INFO_LICENSE != 0 {
        print(req, LICENSE);
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
) -> Result<OrFalse<PhpString>, Thrown> {
    let Some(name) = extension else {
        return PhpString::new(req, PHP_VERSION.as_bytes()).map(OrFalse::Value);
    };
    let Some(extension) = req.extension_named(name.as_bytes()) else {
        return Ok(OrFalse::False);
    };
    let version = req.extension_version(extension).unwrap_or_default();
    PhpString::new(req, version.as_bytes()).map(OrFalse::Value)
}

/// `php_print_credits(flag)` (`Z/ext/standard/credits.c:26`) as text.
fn print_credits(req: &mut Request, flag: i64) {
    print(req, "PHP Credits\n");
    if flag & CREDITS_GROUP != 0 {
        table(req, &[&["PHP Group"], &[GROUP]]);
    }
    if flag & CREDITS_GENERAL != 0 {
        table(req, &[&["Language Design & Concept"], &[DESIGN]]);
        print(req, "\n");
        colspan_header(req, "PHP Authors");
        row(req, &["Contribution", "Authors"]);
        for (what, who) in AUTHORS {
            row(req, &[what, who]);
        }
    }
    if flag & CREDITS_SAPI != 0 {
        print(req, "\n");
        colspan_header(req, "SAPI Modules");
        row(req, &["Contribution", "Authors"]);
        for (what, who) in SAPI_CREDITS {
            row(req, &[what, who]);
        }
    }
    if flag & CREDITS_MODULES != 0 {
        print(req, "\n");
        colspan_header(req, "Module Authors");
        row(req, &["Module", "Authors"]);
        for (what, who) in MODULE_CREDITS {
            row(req, &[what, who]);
        }
    }
    if flag & CREDITS_DOCS != 0 {
        print(req, "\n");
        colspan_header(req, "PHP Documentation");
        for (what, who) in DOCS {
            row(req, &[what, who]);
        }
    }
    if flag & CREDITS_QA != 0 {
        table(req, &[&["PHP Quality Assurance Team"], &[QA]]);
    }
    if flag & CREDITS_WEB != 0 {
        print(req, "\n");
        colspan_header(req, "Websites and Infrastructure team");
        for (what, who) in WEB {
            row(req, &[what, who]);
        }
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
fn php_sapi_name(req: &mut Request) -> Result<OrFalse<PhpString>, Thrown> {
    let name = req.sapi_name().to_owned();
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
) -> Result<PhpString, Thrown> {
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

/// `php_ini_scanned_files(): string|false`,
/// `PHP_FUNCTION(php_ini_scanned_files)` (`Z/ext/standard/info.c:1336`):
/// the engine scans no directory for ini files, so `false`.
///
/// ```php
/// var_dump(php_ini_scanned_files());
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn php_ini_scanned_files() -> OrFalse<PhpString> {
    OrFalse::False
}

/// `php_ini_loaded_file(): string|false`,
/// `PHP_FUNCTION(php_ini_loaded_file)` (`Z/ext/standard/info.c:1349`):
/// the engine reads no `php.ini`, so `false`, as php's `-n` has it.
///
/// ```php
/// var_dump(php_ini_loaded_file());
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
fn php_ini_loaded_file() -> OrFalse<PhpString> {
    OrFalse::False
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

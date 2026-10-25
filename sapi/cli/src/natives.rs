//! What php's CLI and its built-in server hand `php_module_startup` beside
//! the bundle, declared here over `php_ext` as an extension declares its
//! own (ADR-0008 as amended by #1348): the CLI's `additional_functions`,
//! `dl()` and the process title's two (`Z/sapi/cli/php_cli.c:449-453`),
//! which php registers under `standard`; and the built-in server's
//! `cli_server_module_entry`, named `cli_server`, with its one entry, and
//! its `server_additional_functions` in the CLI's place
//! (`Z/sapi/cli/php_cli_server.c:463-510`, `Z/sapi/cli/php_cli.c:1302`).
//! The process title is the memory `ps` shows the command line from,
//! which the host saved before it read its arguments (#1175), so its
//! natives and the area are this crate's alone.
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Ini, Request};
use php_ext::values::{Owner, PhpArray, PhpString};
use php_sapi::EngineBuilder;

/// The memory `ps` shows a process's command line from, which php's CLI
/// takes over in `save_ps_args` (`Z/sapi/cli/ps_title.c:147`) before it
/// reads its arguments: the `argv` strings and the environment strings
/// after them, each byte written atomically, so every request of the
/// built-in server may hold it at once without waiting on another; and
/// the length of the title last set, `ps_buffer_cur_len`.
#[derive(Debug)]
pub(crate) struct ProcessTitle {
    area: &'static [AtomicU8],
    shown: AtomicUsize,
}

impl ProcessTitle {
    /// The title over `area`, `ps_buffer` to `ps_buffer_size`, with no
    /// title set yet.
    pub(crate) fn new(area: &'static [AtomicU8]) -> ProcessTitle {
        ProcessTitle {
            area,
            shown: AtomicUsize::new(0),
        }
    }

    /// `set_ps_title` (`Z/sapi/cli/ps_title.c:345`): a title as long as
    /// the area or longer is `Too long`; else it is written with its NUL
    /// and the rest of the area padded with NULs, as Linux and macOS pad
    /// it.
    fn set(&self, title: &[u8]) -> Result<(), &'static str> {
        if title.len() >= self.area.len() {
            return Err("Too long");
        }
        let padding = std::iter::repeat_n(0, self.area.len() - title.len());
        for (cell, byte) in self.area.iter().zip(title.iter().copied().chain(padding)) {
            cell.store(byte, Ordering::Relaxed);
        }
        self.shown.store(title.len(), Ordering::Relaxed);
        Ok(())
    }

    /// `get_ps_title`: the area up to the title last set.
    fn get(&self) -> Vec<u8> {
        let shown = self.shown.load(Ordering::Relaxed).min(self.area.len());
        self.area[..shown]
            .iter()
            .map(|cell| cell.load(Ordering::Relaxed))
            .collect()
    }
}

/// `builder` with the CLI's `additional_functions`.
#[must_use]
pub(crate) fn cli(builder: EngineBuilder) -> EngineBuilder {
    builder.additional_functions(vec![
        dl::declared(),
        cli_set_process_title::declared(),
        cli_get_process_title::declared(),
    ])
}

/// `builder` with the built-in server's module, `cli_server.color` off and
/// nothing else, and its `server_additional_functions`.
#[must_use]
pub(crate) fn server(builder: EngineBuilder) -> EngineBuilder {
    builder
        .additional_extension(server_extension())
        .additional_functions(vec![
            cli_set_process_title::declared(),
            cli_get_process_title::declared(),
            apache_request_headers::declared(),
            apache_response_headers::declared(),
            getallheaders::declared(),
        ])
}

/// `cli_server_module_entry`: its one `PHP_INI_BEGIN` row and its `MINFO`.
fn server_extension() -> Extension {
    ExtensionBuilder::default()
        .ini(vec![IniDecl {
            name: Name(COLOR.as_bytes().into()),
            default: Some(b"0".as_slice().into()),
            user: true,
            per_dir: true,
            system: true,
        }])
        .info(server_info)
        .build("cli_server")
}

/// The built-in server's one entry, a `STD_PHP_INI_BOOLEAN`
/// (`Z/sapi/cli/php_cli_server.c:469`).
const COLOR: &str = "cli_server.color";

/// `PHP_MINFO_FUNCTION(cli_server)` (`Z/sapi/cli/php_cli_server.c:485`):
/// `DISPLAY_INI_ENTRIES()` alone, `cli_server.color` through
/// `zend_ini_boolean_displayer_cb`, `On` or `Off` in both columns, so the
/// server writes no `Version` row.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/srv' . bin2hex(random_bytes(8));
/// mkdir($dir);
/// file_put_contents("$dir/index.php", '<?php ini_set("cli_server.color", "0"); (new ReflectionExtension("cli_server"))->info();');
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'cli_server.color=1', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
/// $ready = [$pipes[2]]; $none = null;
/// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
/// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
/// echo file_get_contents("http://127.0.0.1:$port/");
/// proc_terminate($p); proc_close($p);
/// unlink("$dir/index.php"); rmdir($dir);
/// ```
/// ```expect process
/// <h2><a name="module_cli_server" href="#module_cli_server">cli_server</a></h2>
/// <table>
/// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
/// <tr><td class="e">cli_server.color</td><td class="v">Off</td><td class="v">On</td></tr>
/// </table>
/// ```
fn server_info(req: &mut Request) {
    let shown = |value: Option<&[u8]>| {
        let on = Ini::parse_bool(value.unwrap_or_default());
        if on { "On" } else { "Off" }
    };
    let entries = req.ini().entries();
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.decl.name.0.as_ref() == COLOR.as_bytes())
    else {
        return;
    };
    let (local, master) = (
        shown(entry.value.as_deref()),
        shown(entry.master.as_deref()),
    );
    req.ini_mut().info_ini_table(&[[COLOR, local, master]]);
}

/// `cli_set_process_title(string $title): bool`
/// (`Z/sapi/cli/php_cli_process_title.c:26`): the title `ps` shows set
/// over the area the host saved, and `true`; a title as long as the area
/// or longer, or no area saved, is `set_ps_title`'s error in php's
/// warning, and `false`. Before any title is set the one read is empty.
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
    let set = crate::process_title().map_or(Err("Too long"), |area| area.set(title.as_bytes()));
    match set {
        Ok(()) => true,
        Err(why) => {
            docref(
                req,
                Level::Warning,
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
fn cli_get_process_title(req: &mut Request) -> Result<Option<Owner<PhpString>>, Thrown> {
    let Some(title) = crate::process_title().map(ProcessTitle::get) else {
        docref(
            req,
            Level::Warning,
            "cli_get_process_title had an error: Not initialized correctly",
        );
        return Ok(None);
    };
    PhpString::new(req, &title).map(Some)
}

/// `dl(string $extension_filename): bool`, `ext/standard`'s, which the CLI
/// names first in its `additional_functions` ([`Extensions::dl`]): with
/// `enable_dl` on, a name with a `/` or of `MAXPATHLEN` bytes refused in
/// php's words, and `false`.
///
/// ```php
/// var_dump(dl("/path/to/module"), dl(str_repeat("a", 2000)));
/// ```
/// ```expect process
/// Warning: dl(): Temporary module name should contain only filename in %s on line 2
///
/// Warning: dl(): Filename exceeds the maximum allowed length of 1024 characters in %s on line 2
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// echo new ReflectionFunction("dl");
/// ```
/// ```expect process
/// Function [ <internal:standard> function dl ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $extension_filename ]
///   }
///   - Return [ bool ]
/// }
/// ```
///
/// php-fpm and phpdbg declare no `additional_functions`:
///
/// ```php
/// var_dump(function_exists("dl"));
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// bool(false)
/// ```
///
/// ```php
/// var_dump(function_exists("dl"));
/// ```
/// ```request phpdbg
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> bool(false)
/// [Script ended normally]
/// prompt>
/// ```
#[php_function]
fn dl(req: &mut Request, extension_filename: PhpString) -> bool {
    req.extensions_mut().dl(extension_filename.as_bytes())
}

/// `apache_request_headers(): array`, also `getallheaders()`
/// (`Z/sapi/cli/php_cli_server.c:407`): the request's headers as the
/// client sent them, each name as it spelt it, a name repeated in one
/// spelling once with the values joined; the server's alone, so the CLI
/// declares neither:
///
/// ```php
/// var_dump(function_exists("getallheaders"), function_exists("apache_request_headers"));
/// ```
/// ```expect process
/// bool(false)
/// bool(false)
/// ```
#[php_function(aliases(getallheaders))]
fn apache_request_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.variables_mut().request_headers()
}

/// `apache_response_headers(): array` (`Z/sapi/cli/php_cli_server.c:425`):
/// the header list as name and value ([`Response::response_headers`]); the
/// server's alone, so the CLI declares none:
///
/// ```php
/// var_dump(function_exists("apache_response_headers"));
/// ```
/// ```expect process
/// bool(false)
/// ```
#[php_function]
fn apache_response_headers(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    req.response_mut().response_headers()
}

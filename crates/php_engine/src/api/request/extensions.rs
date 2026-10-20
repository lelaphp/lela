//! The bundle's extensions as a request reads them: the loaded
//! extensions and Zend extensions, each one's name, version, number,
//! functions, classes, constants and dependencies, the multibyte
//! provider, the extension whose `rinit` is being served, `dl()`, and
//! phpinfo's tables as `php_info_print_module` and `info.c` print them.
//! The area holds no state of its own; it reads the engine's shared
//! tables.

use std::fmt::Write as _;

use php_unit::decl::Name;

use crate::api::abi::{Dependency, ExtensionEntry, ZendExtension};
use crate::api::call::Function;
use crate::api::raising::Level;
use crate::api::values::ClassId;

use super::{ExtensionId, Ini, Request};

/// The request's extensions facet (design §4.4): the bundle as the
/// request sees it: each extension's name, version, natives, constants,
/// ini and `phpinfo()` rows, `dl()`, and the multibyte provider.
/// [`Request::extensions`] reads it and [`Request::extensions_mut`]
/// changes it; either is the whole request, since nearly every operation
/// can serve PHP code.
pub struct Extensions<R>(pub(crate) R);

impl Request {
    /// The extensions facet, to read.
    #[must_use]
    pub fn extensions(&self) -> Extensions<&Request> {
        Extensions(self)
    }

    /// The extensions facet, to change.
    pub fn extensions_mut(&mut self) -> Extensions<&mut Request> {
        Extensions(self)
    }
}

/// One row of a table on phpinfo's page, as `info.c`'s printers write
/// it ([`Extensions::info_rows`]).
#[derive(Clone, Copy)]
pub enum InfoRow<'a> {
    /// `php_info_print_table_header`: the cells as they are.
    Header(&'a [&'a str]),
    /// `php_info_print_table_row`: the cells, escaped in HTML.
    Row(&'a [&'a str]),
    /// `php_info_print_table_colspan_header(columns, header)`.
    Colspan(usize, &'a str),
    /// Bytes the caller wrote for the form the host prints, as
    /// `php_print_gpcse_array` and `display_ini_entries` write their rows
    /// between a table's start and its end.
    Written(&'a [u8]),
}

/// `php_info_print_html_esc` (`Z/ext/standard/info.c:52`):
/// `php_escape_html_entities(str, len, 0, ENT_QUOTES, "utf-8")`, the five
/// characters `htmlspecialchars` escapes, `'` as `&#039;`, and nothing at
/// all for text that is not UTF-8, which the escaper refuses whole.
fn info_html_esc(text: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(text).is_err() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(text.len());
    for &b in text {
        match b {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'"' => out.extend_from_slice(b"&quot;"),
            b'\'' => out.extend_from_slice(b"&#039;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            _ => out.push(b),
        }
    }
    out
}

/// `fmt::Write` into bytes, for the rows [`Extensions::info_rows`] formats.
struct Bytes<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Bytes<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

impl Request {
    pub(super) fn extension_of(&self, extension: ExtensionId) -> Option<&ExtensionEntry> {
        self.shared
            .extensions
            .get(usize::try_from(extension.0).ok()?)
    }
}

impl<'r> Extensions<&'r Request> {
    /// `get_loaded_extensions()`: `Core`, then the bundle in `MINIT`
    /// order, each the name its `zend_module_entry` carries.
    #[must_use]
    pub fn loaded_extensions(self) -> Vec<String> {
        (self.0.shared.listed())
            .map(|(_, e)| e.name.clone())
            .collect()
    }

    /// `zend_extensions`, the Zend extensions in the order they were
    /// loaded, each its `zend_extension` row: what
    /// `get_loaded_extensions(true)` lists and `ReflectionZendExtension`
    /// finds by name.
    #[must_use]
    pub fn zend_extensions(self) -> Vec<ZendExtension> {
        self.0
            .shared
            .extensions
            .iter()
            .filter_map(|e| e.zend.clone())
            .collect()
    }

    /// `zend_hash_str_find_ptr(&module_registry, lcname)`: the linked
    /// extension named, php's table being keyed by the lowered name;
    /// none for a name the engine has not. `Core` is the engine's own
    /// natives, classes, constants and ini entries.
    ///
    /// ```php
    /// $core = new ReflectionExtension('core');
    /// var_dump($core->getName(), get_loaded_extensions()[0]);
    /// var_dump((new ReflectionConstant('E_ALL'))->getExtensionName());
    /// var_dump((new ReflectionFunction('strlen'))->getExtensionName());
    /// var_dump((new ReflectionClass('stdClass'))->getExtension()->name);
    /// var_dump(isset($core->getINIEntries()['precision']), isset($core->getFunctions()['strlen']));
    /// ```
    /// ```expect
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// string(4) "Core"
    /// bool(true)
    /// bool(true)
    /// ```
    #[must_use]
    pub fn named(self, name: &[u8]) -> Option<ExtensionId> {
        let wanted = name.to_ascii_lowercase();
        (self.0.shared.listed())
            .find(|(_, e)| e.name.as_bytes().to_ascii_lowercase() == wanted)
            .and_then(|(i, _)| u32::try_from(i).ok())
            .map(ExtensionId)
    }

    /// `module->name`: the name an extension registered under, as its
    /// `zend_module_entry` spells it.
    #[must_use]
    pub fn name(self, extension: ExtensionId) -> Option<&'r str> {
        self.0.extension_of(extension).map(|e| e.name.as_str())
    }

    /// `zend_multibyte_get_functions()`: the extension that handed the
    /// engine its encoding functions
    /// ([`crate::api::abi::ExtensionBuilder::encodings`]), the last in
    /// bundle order as a later `zend_multibyte_set_functions` replaces
    /// an earlier one; none where no extension did, and Zend's dummies
    /// answer. Its name is php's `provider_name`.
    #[must_use]
    pub fn multibyte_provider(self) -> Option<ExtensionId> {
        self.0.shared.supplied.encodings.supplier()
    }

    /// `module->version`: what the extension declared with
    /// `ExtensionBuilder::version`, else `PHP_VERSION`, as `Core`'s is.
    #[must_use]
    pub fn version(self, extension: ExtensionId) -> Option<&'static str> {
        self.0.extension_of(extension).map(|e| e.version)
    }

    /// `module->module_number`, `zend_next_free_module` at registration:
    /// `Core` takes zero at `zend_startup` and each extension the next
    /// number in startup order.
    #[must_use]
    pub fn number(self, extension: ExtensionId) -> u32 {
        extension.0
    }

    /// The natives an extension declared, in the order it declared
    /// them: what `CG(function_table)` holds with
    /// `internal_function.module` naming it, so none `disable_functions`
    /// took out.
    #[must_use]
    pub fn functions(self, extension: ExtensionId) -> Vec<Function> {
        (self.0.shared.function_modules.iter().enumerate())
            .filter(|&(at, module)| {
                *module == Some(extension) && self.0.shared.in_function_table(at)
            })
            .filter_map(|(at, _)| Some(Function(u32::try_from(at).ok()?)))
            .collect()
    }

    /// `internal_function.module`: the extension that declared a native
    /// function, `Core` for Zend's own and `standard` for a native of
    /// php's `standard` the engine declares itself where the bundle has
    /// it; for a native method, the extension of the class that declared
    /// it, which `zend_register_functions` writes onto each of a class's
    /// methods, so a native trait's method copied into a user class
    /// keeps the trait's (`zend_add_trait_method` copies the whole
    /// `zend_internal_function`); none for a native of `standard` in an
    /// engine without it, for a trampoline no extension declares and for
    /// a user function.
    #[must_use]
    pub fn function_extension(self, function: Function) -> Option<ExtensionId> {
        let function = self.0.served_native(function);
        let at = usize::try_from(function.0).ok()?;
        if let Some(module) = self.0.shared.function_modules.get(at) {
            return *module;
        }
        match self.0.native_method(function)?.at {
            crate::engine::MethodAt::Declared { class, .. } => {
                self.0.extensions().class_extension(class)
            }
            _ => None,
        }
    }

    /// `ce->info.internal.module`: the extension that declared a native
    /// class, `Core` for one Zend declares itself, as
    /// `ReflectionClass::getExtension` reads it; none for a user class.
    #[must_use]
    pub fn class_extension(self, class: ClassId) -> Option<ExtensionId> {
        let at = usize::try_from(class.0).ok()?;
        self.0.shared.class_modules.get(at).copied()
    }

    /// `ce->info.internal.module->name` as `_class_string` and
    /// `_function_string` print it after `<internal:`: the name of the
    /// extension that declared a native class, `Core` for one Zend
    /// declares itself, and none for a user class.
    #[must_use]
    pub fn class_extension_name(self, class: ClassId) -> Option<&'r str> {
        self.0
            .extensions()
            .class_extension(class)
            .and_then(|extension| self.0.extensions().name(extension))
    }

    /// `ZEND_CONSTANT_MODULE_NUMBER(constant) == module->module_number`
    /// over `EG(zend_constants)`: the names of the constants registered
    /// under an extension, in the table's order, each one
    /// `api::call::constant` reads the value of; empty for an extension
    /// that has none. The table holds the engine's own constants first,
    /// as `zend_register_standard_constants` runs before any `MINIT`, with
    /// `PHP_SAPI` before `PHP_BINARY` where `php_module_startup` registers
    /// it, then each extension's in bundle order (`Engine::build`), then
    /// what was registered while the request ran: a host's under `Core`,
    /// as the CLI's `PHP_CLI_PROCESS_TITLE` and its three streams, and an
    /// extension's under its own.
    #[must_use]
    pub fn constants(self, extension: ExtensionId) -> Vec<Name> {
        let Some(module) = self.0.extensions().name(extension) else {
            return Vec::new();
        };
        self.0
            .constant_table()
            .into_iter()
            .filter(|(owner, _, _)| *owner == Some(module))
            .map(|(_, name, _)| Name(name.to_vec().into()))
            .collect()
    }

    /// `module->deps`: the rows an extension declared through
    /// `ExtensionBuilder::dependencies`, in their order; the empty
    /// table where php's pointer is null, which is every extension that
    /// declares none.
    #[must_use]
    pub fn extension_dependencies(self, extension: ExtensionId) -> Vec<Dependency> {
        self.0
            .extension_of(extension)
            .map(|e| e.dependencies.clone())
            .unwrap_or_default()
    }

    /// `sapi_module.phpinfo_as_text`: whether the host prints phpinfo's
    /// page as text, which the CLI does and a host serving the web, whose
    /// page is `info.c`'s HTML, does not.
    #[must_use]
    pub fn phpinfo_as_text(self) -> bool {
        self.0.web().is_none()
    }
}

impl Extensions<&mut Request> {
    /// `zend_multibyte_get_functions`: the encoding functions the last
    /// extension in bundle order handed the engine
    /// ([`ExtensionBuilder::encodings`](crate::api::abi::ExtensionBuilder::encodings)),
    /// bound to this request as its settings stand, or Zend's dummies,
    /// which fetch and convert nothing, where none did. What an
    /// extension's `zend_multibyte_fetch_encoding` and
    /// `zend_multibyte_encoding_converter` reach, as `exif.c`'s comment
    /// decoding does.
    pub fn encodings(self) -> Box<dyn crate::api::abi::Multibyte> {
        crate::multibyte::bind(self.0)
    }

    /// `zend_multibyte_parse_encoding_list`: the encodings `names`
    /// lists, as the functions parse an ini entry's list, `None` after
    /// the provider's warning where a name names none. Zend's dummies
    /// parse every list, to none.
    pub fn parse_encoding_list(self, names: &[u8]) -> Option<Vec<crate::api::abi::Encoding>> {
        match self.0.extensions_mut().encodings().list(names) {
            Ok(list) => Some(list),
            Err(name) => {
                let name = String::from_utf8_lossy(&name);
                let message = format!("INI setting contains invalid encoding \"{name}\"");
                crate::api::raising::docref(self.0, Level::Warning, &message);
                None
            }
        }
    }

    /// `php_info_print_module` (`Z/ext/standard/info.c:133`): the name,
    /// as a one-column table in text and as the `module_<name>` anchor
    /// `<h2>` in HTML, then the extension's `MINFO`; for an extension that
    /// declared none, a `Version` row and `DISPLAY_INI_ENTRIES`. Every
    /// extension has a version, so php's other branch, the bare name, is
    /// never taken. What `phpinfo()`,
    /// `ReflectionExtension::info` and `php --ri` print.
    pub fn info(self, extension: ExtensionId) {
        let Some(module) = self.0.extension_of(extension).cloned() else {
            return;
        };
        if self.0.extensions().phpinfo_as_text() {
            self.0
                .extensions_mut()
                .info_rows(&[InfoRow::Header(&[module.name.as_str()])]);
        } else {
            // `php_url_encode` of the name, lowered: the anchor's name.
            let mut anchor = String::new();
            for b in module.name.to_ascii_lowercase().bytes() {
                match b {
                    b' ' => anchor.push('+'),
                    b'-' | b'.' | b'_' => anchor.push(char::from(b)),
                    _ if b.is_ascii_alphanumeric() => anchor.push(char::from(b)),
                    _ => {
                        write!(anchor, "%{b:02X}").ok();
                    }
                }
            }
            let heading = format!(
                "<h2><a name=\"module_{anchor}\" href=\"#module_{anchor}\">{}</a></h2>\n",
                module.name
            );
            self.0.response_mut().write(heading.as_bytes());
        }
        if let Some(info) = module.info {
            info(self.0);
            return;
        }
        self.0
            .extensions_mut()
            .info_table(&[&["Version", module.version]]);
        let names: Vec<&[u8]> = module.ini.iter().map(|entry| &*entry.name.0).collect();
        self.0.ini_mut().info_ini_entries(&names);
    }

    /// `php_info_print_table_start`, one `php_info_print_table_row` per
    /// row and `php_info_print_table_end` ([`Extensions::info_rows`]):
    /// what an extension's `MINFO` writes its table through.
    pub fn info_table(self, rows: &[&[&str]]) {
        let rows: Vec<InfoRow<'_>> = rows.iter().map(|row| InfoRow::Row(row)).collect();
        self.0.extensions_mut().info_rows(&rows);
    }

    /// One table of phpinfo's page as `info.c` writes it in the form the
    /// host prints (`Z/ext/standard/info.c:1062-1220`): in text a blank
    /// line, a header's cells joined by ` => ` with an empty one a
    /// space, a row's the same but an empty cell a space with nothing
    /// after it, a colspan header centred in 74 columns; in HTML
    /// `<table>`, a header `<tr class="h">` of `<th>` cells as they are, a
    /// row `<tr>` of cells `class="e"` then `class="v"`, each escaped as
    /// `php_info_print_html_esc` escapes it and closed by ` </td>`, an
    /// empty one `<i>no value</i>`, and `</table>`.
    ///
    /// ```php
    /// ob_start(); (new ReflectionExtension('json'))->info(); echo ob_get_clean();
    /// ```
    /// ```request
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// <h2><a name="module_json" href="#module_json">json</a></h2>
    /// <table>
    /// <tr><td class="e">json support </td><td class="v">enabled </td></tr>
    /// </table>
    /// ```
    pub fn info_rows(self, rows: &[InfoRow<'_>]) {
        let text = self.0.extensions().phpinfo_as_text();
        let mut out = Vec::new();
        out.extend_from_slice(if text { b"\n" } else { b"<table>\n" });
        for row in rows {
            match (row, text) {
                (InfoRow::Header(cells), true) => {
                    let last = cells.len().saturating_sub(1);
                    for (i, cell) in cells.iter().enumerate() {
                        out.extend_from_slice(if cell.is_empty() { " " } else { cell }.as_bytes());
                        out.extend_from_slice(if i < last { b" => " } else { b"\n" });
                    }
                }
                (InfoRow::Header(cells), false) => {
                    out.extend_from_slice(b"<tr class=\"h\">");
                    for cell in *cells {
                        out.extend_from_slice(b"<th>");
                        out.extend_from_slice(if cell.is_empty() { " " } else { cell }.as_bytes());
                        out.extend_from_slice(b"</th>");
                    }
                    out.extend_from_slice(b"</tr>\n");
                }
                (InfoRow::Row(cells), true) => {
                    let last = cells.len().saturating_sub(1);
                    for (i, cell) in cells.iter().enumerate() {
                        if cell.is_empty() {
                            out.push(b' ');
                        } else {
                            out.extend_from_slice(cell.as_bytes());
                            if i < last {
                                out.extend_from_slice(b" => ");
                            }
                        }
                        if i == last {
                            out.push(b'\n');
                        }
                    }
                }
                (InfoRow::Row(cells), false) => {
                    out.extend_from_slice(b"<tr>");
                    for (i, cell) in cells.iter().enumerate() {
                        let class = if i == 0 { "e" } else { "v" };
                        write!(Bytes(&mut out), "<td class=\"{class}\">").ok();
                        if cell.is_empty() {
                            out.extend_from_slice(b"<i>no value</i>");
                        } else {
                            out.extend_from_slice(&info_html_esc(cell.as_bytes()));
                        }
                        out.extend_from_slice(b" </td>");
                    }
                    out.extend_from_slice(b"</tr>\n");
                }
                (InfoRow::Colspan(columns, header), true) => {
                    let _ = columns;
                    let spaces = 74_usize.saturating_sub(header.len()) / 2;
                    let pad = " ".repeat(spaces.max(1));
                    writeln!(Bytes(&mut out), "{pad}{header}{pad}").ok();
                }
                (InfoRow::Colspan(columns, header), false) => {
                    writeln!(
                        Bytes(&mut out),
                        "<tr class=\"h\"><th colspan=\"{columns}\">{header}</th></tr>"
                    )
                    .ok();
                }
                (InfoRow::Written(bytes), _) => out.extend_from_slice(bytes),
            }
        }
        if !text {
            out.extend_from_slice(b"</table>\n");
        }
        self.0.response_mut().write(&out);
    }

    /// `PHP_FUNCTION(dl)`, which php exports `PHPAPI` for each host to
    /// name in its `additional_functions`, and `php_load_extension`'s
    /// checks before it opens anything (`Z/ext/standard/dl.c`):
    /// `enable_dl` off is php's warning, as is a name of `MAXPATHLEN`
    /// bytes or more and a name with a `/` in it, since what `dl()` loads
    /// is a file name under `extension_dir`; each answers `false`. A name
    /// that passes is php's `php_dl` without `HAVE_LIBDL`
    /// (`Z/ext/standard/dl.c:261`), its warning, and `false`: this engine
    /// links its extensions at build and has no loader for a C library
    /// built against php's extension API (`trace:dl-no-loader`).
    pub fn dl(self, extension_filename: &[u8]) -> bool {
        if !self.0.ini().get(b"enable_dl").is_some_and(Ini::parse_bool) {
            crate::api::raising::docref(
                self.0,
                Level::Warning,
                "Dynamically loaded extensions aren't enabled",
            );
            return false;
        }
        let max = crate::virtual_cwd::MAXPATHLEN;
        let text = if extension_filename.len() >= max {
            format!("Filename exceeds the maximum allowed length of {max} characters")
        } else if extension_filename.contains(&b'/') {
            String::from("Temporary module name should contain only filename")
        } else {
            format!(
                "Cannot dynamically load {} - dynamic modules are not supported",
                String::from_utf8_lossy(extension_filename)
            )
        };
        crate::api::raising::docref(self.0, Level::Warning, &text);
        false
    }
}

//! The `libxml` extension: `php-src/ext/libxml` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, the platform's libxml2,
//! bound in [`ffi`] alone; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/libxml/tests`, through `xtask phpt --subset`.
//!
//! Eight natives, the `LIBXML_*` constants and `LibXMLError`, over the
//! per-request error state every XML extension reads: `LIBXML(error_list)`,
//! the list `libxml_use_internal_errors(true)` keeps, and the buffer the
//! handlers join a message in. Beside `extension()` the crate exports
//! what `php_libxml.h` declares `PHPAPI` to the extensions that include
//! it (design §4.10): [`ffi`]'s node objects, frees and serialisers, and
//! [`report`], [`issue_error`] and [`uses_internal_errors`] over the
//! errors [`ffi::capture`] copied out of a call.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

pub mod ffi;
mod image_svg;
mod load;
mod mime_sniff;

use php_ext::abi::{
    Callable, Dependency, DependencyKind, Extension, ExtensionBuilder, Instance, OrFalse, Raw,
    True, Untyped,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::write_prop;
use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, arg_type_error, docref};
use php_ext::request::{Request, StreamCtx};
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};

use ffi::{Raised, Route, sys};
pub use mime_sniff::sniff_charset_from_stream;

/// Request-scoped: built at request start, dropped after `post_deactivate`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. php's `zend_libxml_globals`.
#[derive(Default)]
struct State {
    /// `LIBXML(error_list)`: the list, while internal errors are on.
    errors: Option<Vec<Raised>>,
    /// `LIBXML(error_buffer)`: a message the handlers joined until its
    /// newline came.
    buffer: Vec<u8>,
    /// `LIBXML(entity_loader_callback)`, and the value it was read off,
    /// and its owner.
    entity_loader: Option<Owner<Callable>>,
    /// `LIBXML(entity_loader_disabled)`.
    entity_loader_disabled: bool,
    /// `LIBXML(stream_context)`, its owner.
    stream_context: Option<Owner<Value>>,
}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('libxml'), LIBXML_VERSION, LIBXML_DOTTED_VERSION, LIBXML_NOEMPTYTAG);
/// print_r((new ReflectionExtension('libxml'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// int(20913)
/// string(6) "2.9.13"
/// int(4)
/// Array
/// (
///     [standard] => Required
/// )
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            libxml_set_streams_context::declared(),
            libxml_use_internal_errors::declared(),
            libxml_get_last_error::declared(),
            libxml_get_errors::declared(),
            libxml_clear_errors::declared(),
            libxml_disable_entity_loader::declared(),
            libxml_set_external_entity_loader::declared(),
            libxml_get_external_entity_loader::declared(),
        ])
        .constants(constants::declared())
        .class::<LibXMLError>()
        .dependencies(vec![Dependency {
            name: String::from("standard"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .register(image_svg::SVG)
        .process_state(|| Ok(initialize()))
        .request_state::<State>()
        .rshutdown(rshutdown)
        .post_deactivate(post_deactivate)
        .info(libxml_info)
        .build("libxml")
}

/// The process scope's proof that libxml2 is started (ADR-0010): built
/// once per process, never ended, as php never calls `xmlCleanupParser`.
struct Initialized;

/// `php_libxml_initialize` (`Z/ext/libxml/libxml.c:934-958`), which
/// php's `MINIT` runs once per process: `xmlInitParser` and, as php's
/// threaded build does, the schema type tables, then php's external
/// entity loader set over the library's default, for every parse of the
/// process. Its table of exports is `ExtensionBuilder::register`'s. It
/// cannot fail.
fn initialize() -> Initialized {
    ffi::Library::init();
    ffi::Library::init_types();
    ffi::Library::install_loader();
    Initialized
}

/// `PHP_MINFO_FUNCTION(libxml)`.
///
/// ```php
/// (new ReflectionExtension('libxml'))->info();
/// ```
/// ```expect
/// libxml
///
/// libXML support => active
/// libXML Compiled Version => 2.9.13
/// libXML Loaded Version => 20913
/// libXML streams => enabled
/// ```
fn libxml_info(req: &mut Request) {
    let loaded = String::from_utf8_lossy(&ffi::Library::loaded_version()).into_owned();
    req.info_table(&[
        &["libXML support", "active"],
        &["libXML Compiled Version", DOTTED_VERSION],
        &["libXML Loaded Version", &loaded],
        &["libXML streams", "enabled"],
    ]);
}

/// `PHP_RSHUTDOWN_FUNCTION(libxml)` (`Z/ext/libxml/libxml.c:1039-1046`):
/// the loader given up, and with it the value
/// `libxml_set_streams_context()` kept, which php leaves to the resource
/// list: both are values of the request's, given up while its heap
/// still stands.
fn rshutdown(req: &mut Request) {
    let state = req.state::<State>();
    let loader = state.entity_loader.take();
    let context = state.stream_context.take();
    if let Some(loader) = loader {
        loader.give_up(req);
    }
    if let Some(value) = context {
        value.give_up(req);
    }
}

/// `php_libxml_post_deactivate` (`Z/ext/libxml/libxml.c:1061-1083`),
/// after `shutdown_executor`: the error list and the message buffer
/// dropped, and the library's last error, which is the thread's, reset,
/// so the thread's next request starts clean.
fn post_deactivate(req: &mut Request) {
    let state = req.state::<State>();
    state.errors = None;
    state.buffer.clear();
    ffi::Raised::reset_last();
}

/// `LIBXML_DOTTED_VERSION` of the headers this binding was written to,
/// which the platform's library is (`build.rs` holds it to 2.9).
const DOTTED_VERSION: &str = "2.9.13";

/// `register_libxml_symbols`: the constants in `libxml.stub.php`'s order,
/// `LIBXML_NO_XXE` absent as it is below 2.13.
#[php_const]
#[allow(non_snake_case)]
pub(crate) mod constants {
    pub(crate) const LIBXML_VERSION: i64 = 20913;
    pub(crate) const LIBXML_DOTTED_VERSION: &str = super::DOTTED_VERSION;
    /// Read off the library the process loaded, when the builder runs.
    pub(crate) fn LIBXML_LOADED_VERSION() -> String {
        String::from_utf8_lossy(&super::ffi::Library::loaded_version()).into_owned()
    }
    pub(crate) const LIBXML_RECOVER: i32 = super::sys::XML_PARSE_RECOVER;
    pub(crate) const LIBXML_NOENT: i32 = super::sys::XML_PARSE_NOENT;
    pub(crate) const LIBXML_DTDLOAD: i32 = super::sys::XML_PARSE_DTDLOAD;
    pub(crate) const LIBXML_DTDATTR: i32 = super::sys::XML_PARSE_DTDATTR;
    pub(crate) const LIBXML_DTDVALID: i32 = super::sys::XML_PARSE_DTDVALID;
    pub(crate) const LIBXML_NOERROR: i32 = super::sys::XML_PARSE_NOERROR;
    pub(crate) const LIBXML_NOWARNING: i32 = super::sys::XML_PARSE_NOWARNING;
    pub(crate) const LIBXML_NOBLANKS: i32 = super::sys::XML_PARSE_NOBLANKS;
    pub(crate) const LIBXML_XINCLUDE: i32 = super::sys::XML_PARSE_XINCLUDE;
    pub(crate) const LIBXML_NSCLEAN: i32 = super::sys::XML_PARSE_NSCLEAN;
    pub(crate) const LIBXML_NOCDATA: i32 = super::sys::XML_PARSE_NOCDATA;
    pub(crate) const LIBXML_NONET: i32 = super::sys::XML_PARSE_NONET;
    pub(crate) const LIBXML_PEDANTIC: i32 = super::sys::XML_PARSE_PEDANTIC;
    pub(crate) const LIBXML_COMPACT: i32 = super::sys::XML_PARSE_COMPACT;
    pub(crate) const LIBXML_NOXMLDECL: i32 = super::sys::XML_SAVE_NO_DECL;
    pub(crate) const LIBXML_PARSEHUGE: i32 = super::sys::XML_PARSE_HUGE;
    pub(crate) const LIBXML_BIGLINES: i32 = super::sys::XML_PARSE_BIG_LINES;
    pub(crate) const LIBXML_NOEMPTYTAG: i32 = super::NOEMPTYTAG;
    pub(crate) const LIBXML_SCHEMA_CREATE: i64 = 1;
    pub(crate) const LIBXML_HTML_NOIMPLIED: i32 = super::sys::HTML_PARSE_NOIMPLIED;
    pub(crate) const LIBXML_HTML_NODEFDTD: i32 = super::sys::HTML_PARSE_NODEFDTD;
    pub(crate) const LIBXML_ERR_NONE: i32 = super::sys::XML_ERR_NONE;
    pub(crate) const LIBXML_ERR_WARNING: i32 = super::sys::XML_ERR_WARNING;
    pub(crate) const LIBXML_ERR_ERROR: i32 = super::sys::XML_ERR_ERROR;
    pub(crate) const LIBXML_ERR_FATAL: i32 = super::sys::XML_ERR_FATAL;
}

/// `LIBXML_SAVE_NOEMPTYTAG`, php's own bit beside libxml's save options.
pub const NOEMPTYTAG: i32 = 1 << 2;

/// `class LibXMLError` as `libxml.stub.php` declares it: six typed
/// properties and no default, which `php_libxml_create_error_object`
/// writes.
#[php_class]
pub(crate) struct LibXMLError;

#[php_class]
impl LibXMLError {
    #[php(property)]
    const level: i64;
    #[php(property)]
    const code: i64;
    #[php(property)]
    const column: i64;
    #[php(property)]
    const message: PhpString;
    #[php(property)]
    const file: PhpString;
    #[php(property)]
    const line: i64;
}

// ---- the face php_libxml.h declares: the error state

/// `LIBXML(error_list)` and `LIBXML(error_buffer)`: the request's libxml
/// errors, as the extensions that include `php_libxml.h` raise and route
/// them.
pub struct Errors;

impl Errors {
    /// `php_libxml_uses_internal_errors`: whether `libxml_use_internal_errors`
    /// is on, so an error goes to the list and not to the program's page.
    pub fn uses_internal(req: &mut Request) -> bool {
        req.state::<State>().errors.is_some()
    }

    /// `php_libxml_issue_error`: a message php raises itself, onto the list
    /// with internal errors on and as a diagnostic of `level` otherwise.
    pub fn issue(req: &mut Request, level: Level, message: &str) {
        let state = req.state::<State>();
        if let Some(list) = state.errors.as_mut() {
            list.push(Raised::internal(message.as_bytes()));
            return;
        }
        docref(req, level, message);
    }

    /// `php_libxml_pretend_ctx_error_ex`: an error raised as if a parser
    /// of `file` had raised it at `line` and `column`, which lexbor's
    /// parse errors are: onto the list with the position and the file,
    /// or a warning with the text alone, its bytes as they are.
    pub fn pretend(req: &mut Request, file: &[u8], line: i32, column: i32, message: &[u8]) {
        let state = req.state::<State>();
        state.buffer.extend_from_slice(message);
        let mut end = state.buffer.len();
        while end > 0 && state.buffer[end - 1] == b'\n' {
            end -= 1;
        }
        if end == state.buffer.len() {
            return;
        }
        state.buffer.truncate(end);
        let text = std::mem::take(&mut state.buffer);
        if let Some(list) = state.errors.as_mut() {
            let mut error = Raised::internal(&text);
            error.line = line;
            error.column = column;
            error.file = Some(file.to_vec());
            list.push(error);
            return;
        }
        docref(req, Level::Warning, &text);
    }

    /// `php_libxml_ctx_error` from php's own code, the entity loader's:
    /// the message joined to the buffer and, once it ends with its
    /// newline, onto the list with the parser's line and column, or
    /// raised as a warning with the parser's file (`Entity` for none) and
    /// line where the parser has an input and alone where it has none;
    /// nothing is raised while a throwable is pending, `suppressed`. The
    /// external subset a parser loads is named with the parser's place in
    /// the document; a resolver that throws is the error its caller sees,
    /// and one that answers another resource php's `TypeError`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->resolveExternals = true;
    /// libxml_set_external_entity_loader(function ($public, $system, $context) {
    ///     var_dump($context);
    ///     return null;
    /// });
    /// $d->loadXML("<!DOCTYPE r PUBLIC \"-//D\" \"r.dtd\">\n<r/>");
    /// libxml_set_external_entity_loader(function ($public, $system, $context) {
    ///     throw new Exception("from the resolver");
    /// });
    /// try { $d->loadXML('<!DOCTYPE r PUBLIC "-//D" "r.dtd"><r/>'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// libxml_set_external_entity_loader(fn($p, $s, $c) => stream_context_create());
    /// try { $d->loadXML('<!DOCTYPE r PUBLIC "-//D" "r.dtd"><r/>'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(4) {
    ///   ["directory"]=>
    ///   string(%d) "%s/"
    ///   ["intSubName"]=>
    ///   string(1) "r"
    ///   ["extSubURI"]=>
    ///   string(5) "r.dtd"
    ///   ["extSubSystem"]=>
    ///   string(4) "-//D"
    /// }
    ///
    /// Warning: DOMDocument::loadXML(): Failed to load external entity "-//D" in Entity, line: 1 in %s on line 8
    /// from the resolver
    /// DOMDocument::loadXML(): The user entity loader callback "{closure:%s:13}" has returned a resource, but it is not a stream
    /// ```
    pub(crate) fn ctx(
        req: &mut Request,
        site: Option<&ffi::Site>,
        message: &[u8],
        suppressed: bool,
    ) {
        let state = req.state::<State>();
        let mut end = message.len();
        while end > 0 && message[end - 1] == b'\n' {
            end -= 1;
        }
        state.buffer.extend_from_slice(&message[..end]);
        if end == message.len() {
            return;
        }
        let text = std::mem::take(&mut state.buffer);
        let (line, column) = site.map_or((0, 0), |s| (s.line, s.column));
        if let Some(list) = state.errors.as_mut() {
            let mut error = Raised::internal(&text);
            error.line = line;
            error.column = column;
            list.push(error);
            return;
        }
        if suppressed {
            return;
        }
        let message = match site {
            Some(site) => in_input(&text, site.file.as_deref(), line),
            None => text,
        };
        docref(req, Level::Warning, &message);
    }

    /// The errors [`ffi::capture`] copied out of one call, routed as php's
    /// handlers would have met them: onto the list with internal errors on,
    /// `php_list_set_error_structure`; otherwise joined in the buffer until a
    /// message ends with its newline and raised then, as
    /// `php_libxml_internal_error_handler_ex` does, with the text and level
    /// of the handler the error reached. The text is libxml's bytes and the
    /// file the input's name as it was given, each raised as php's `%s`
    /// prints it, a byte that is not UTF-8 among them.
    ///
    /// ```php
    /// set_error_handler(function ($n, $s) { echo $n, " ", bin2hex(strstr($s, " in ") ?: $s), "\n"; return true; });
    /// $d = new DOMDocument;
    /// $d->load("data:,<r>x</b>\xff");
    /// simplexml_load_file("data:,<r/>\xff");
    /// ```
    /// ```expect
    /// 2 20696e20646174613a2c3c723e783c2f623eff2c206c696e653a2031
    /// 2 20696e20646174613a2c3c723e783c2f623eff2c206c696e653a2031
    /// 2 73696d706c65786d6c5f6c6f61645f66696c6528293a20646174613a2c3c722f3eff3a313a20706172736572206572726f72203a20457874726120636f6e74656e742061742074686520656e64206f662074686520646f63756d656e74
    /// 2 73696d706c65786d6c5f6c6f61645f66696c6528293a203c722f3eff
    /// 2 73696d706c65786d6c5f6c6f61645f66696c6528293a20202020205e
    /// ```
    pub fn route(req: &mut Request, raised: Vec<Raised>) {
        for error in raised {
            let state = req.state::<State>();
            if let Some(list) = state.errors.as_mut() {
                list.push(error);
                continue;
            }
            // libxml's own handler writes its report in pieces, one call
            // of the generic handler per line, and php raises each.
            let pieces: Vec<Vec<u8>> = match error.route {
                Route::Silent => continue,
                Route::Default => error
                    .printed
                    .split_inclusive(|b| *b == b'\n')
                    .map(<[u8]>::to_vec)
                    .collect(),
                Route::Parser | Route::Generic => vec![error.message.clone()],
            };
            for piece in pieces {
                Self::piece(req, &error, &piece);
            }
        }
    }

    /// The errors of a call that lent libxml the request, one of
    /// [`ffi::Parser`]'s, [`ffi::Validate`]'s or [`ffi::Save`]'s that
    /// loads or writes, routed as [`Errors::route`] routes
    /// them, and what a program's code threw from inside the call handed
    /// on: the errors raised after the throw reach the list alone, as
    /// php's handlers raise nothing while `EG(exception)` is set.
    pub fn settle(
        req: &mut Request,
        raised: Vec<Raised>,
        thrown: Option<Thrown>,
    ) -> Result<(), Thrown> {
        Self::route(req, raised);
        thrown.map_or(Ok(()), Err)
    }

    /// One call of php's handler: `text` joined to the buffer, and raised
    /// once the buffer ends with a newline.
    fn piece(req: &mut Request, error: &Raised, text: &[u8]) {
        let state = req.state::<State>();
        state.buffer.extend_from_slice(text);
        let mut end = state.buffer.len();
        while end > 0 && state.buffer[end - 1] == b'\n' {
            end -= 1;
        }
        if end == state.buffer.len() {
            return;
        }
        state.buffer.truncate(end);
        let message = std::mem::take(&mut state.buffer);
        match error.route {
            Route::Parser => {
                let level = if error.level == ffi::Level::Warning {
                    Level::Notice
                } else {
                    Level::Warning
                };
                let message = in_input(&message, error.file.as_deref(), error.line);
                docref(req, level, &message);
            }
            Route::Generic | Route::Default | Route::Silent => {
                docref(req, Level::Warning, &message);
            }
        }
    }
}

/// `php_libxml_ctx_error_level`'s `"%s in %s, line: %d"`: the message at
/// the parser's input, named `Entity` where the input has no file. Bytes
/// throughout, as php's `%s` prints a document's file name and libxml's
/// text, so a name that is not UTF-8 is raised as it is.
fn in_input(message: &[u8], file: Option<&[u8]>, line: i32) -> Vec<u8> {
    let line = format!(", line: {line}");
    [message, b" in ", file.unwrap_or(b"Entity"), line.as_bytes()].concat()
}

// ---- the face php_libxml.h declares: the document handlers

/// `php_libxml_document_handlers`: how one document's nodes serialise,
/// which the extension that made the document chooses and every
/// extension that saves one of its nodes calls through (`dom_set_xml_class`
/// gives dom's modern documents their own). Each routes what libxml
/// raised as php's handlers would have met it.
pub trait DocumentHandlers: Sync {
    /// `dump_doc_to_str`: the document with the save `options`, `None`
    /// where a step failed, and what a serialiser threw, php's
    /// `EG(exception)` beside the null it answers.
    fn dump_doc_to_str(
        &self,
        req: &mut Request,
        doc: ffi::Node,
        options: i32,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown>;

    /// `dump_node_to_str`: one node of the document.
    fn dump_node_to_str(
        &self,
        req: &mut Request,
        doc: ffi::Node,
        node: ffi::Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown>;

    /// `dump_doc_to_file`: the bytes written to `path`, `-1` where it
    /// failed.
    fn dump_doc_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        doc: ffi::Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<i64, Thrown>;

    /// `dump_node_to_file`: what closing the file answered, `-1` where it
    /// would not open.
    fn dump_node_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        doc: ffi::Node,
        node: ffi::Node,
        format: bool,
    ) -> Result<i64, Thrown>;
}

/// `php_libxml_default_document_handlers`: libxml's own serialisers.
///
/// ```php
/// $s = simplexml_load_string('<?xml version="1.0"?><r><a>t</a></r>');
/// echo $s->asXML(), $s->a->asXML(), "\n";
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// echo $d->saveXML($d->documentElement), "\n";
/// ```
/// ```expect
/// <?xml version="1.0"?>
/// <r><a>t</a></r>
/// <a>t</a>
/// <r><a/></r>
/// ```
pub struct DefaultHandlers;

impl DocumentHandlers for DefaultHandlers {
    fn dump_doc_to_str(
        &self,
        req: &mut Request,
        doc: ffi::Node,
        options: i32,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let (saved, raised) = ffi::Save::doc(doc, options, no_empty, encoding);
        Errors::route(req, raised);
        Ok(saved)
    }

    fn dump_node_to_str(
        &self,
        req: &mut Request,
        doc: ffi::Node,
        node: ffi::Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let (saved, raised) = ffi::Save::node(doc, node, format, no_empty, encoding);
        Errors::route(req, raised);
        Ok(saved)
    }

    fn dump_doc_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        doc: ffi::Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<i64, Thrown> {
        let (written, raised, thrown) = ffi::Save::file(req, path, doc, encoding, format, no_empty);
        Errors::settle(req, raised, thrown)?;
        Ok(written)
    }

    fn dump_node_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        doc: ffi::Node,
        node: ffi::Node,
        format: bool,
    ) -> Result<i64, Thrown> {
        let (written, raised, thrown) = ffi::Save::node_file(req, path, doc, node, format);
        Errors::settle(req, raised, thrown)?;
        Ok(written)
    }
}

// ---- the face php_libxml.h declares: the exports

/// `php_libxml_export_node`: what an XML extension answers for one of its
/// objects, the node it stands for and the document it counts, or `None`.
pub type ExportNode = fn(&mut Request, PhpObject) -> Option<ffi::Exported>;

/// One row of `php_libxml_exports`: the root class it answers for, by
/// name, and its export. An extension hands it to
/// `ExtensionBuilder::register` at startup, so the table is immutable
/// once the engine is built (ADR-0005).
pub struct Export {
    class: &'static [u8],
    answer: ExportNode,
    half: Option<NodeObjectOf>,
}

/// `Z_LIBXML_NODE_P`, `php_libxml_node_fetch_object`: the libxml half of
/// an object of an XML extension's classes, which php reaches through the
/// object's handler offset, whether or not it stands for a node yet.
pub type NodeObjectOf = fn(&mut Request, PhpObject) -> Option<ffi::Borrowed>;

impl Export {
    /// The row with the way to the libxml half of the extension's
    /// objects, which [`node_object`] reads.
    #[must_use]
    pub fn with_node_object(self, half: NodeObjectOf) -> Export {
        Export {
            half: Some(half),
            ..self
        }
    }
}

/// `php_libxml_register_export(ce, export_function)`: the row for `class`,
/// the root of the classes whose objects `export` answers for.
#[must_use]
pub fn register_export(class: &'static [u8], export: ExportNode) -> Export {
    Export {
        class,
        answer: export,
        half: None,
    }
}

/// The row of the export table that answers for the root of `object`'s
/// class.
fn export_row(req: &mut Request, object: PhpObject) -> Option<(ExportNode, Option<NodeObjectOf>)> {
    let class = req.lineage(object.class()).last()?;
    if req.class_parent(class).is_some() {
        return None;
    }
    let root = req.class_decl(class)?.name.0.clone();
    req.registered::<Export>()
        .find(|row| row.class == &root[..])
        .map(|row| (row.answer, row.half))
}

/// `Z_LIBXML_NODE_P(object)`: the libxml half of an object of any XML
/// extension, through the row registered for the root of its class, as
/// ext/xsl's `transformToDoc` lays a document into an object of the
/// class it was asked for; `None` for an object no row answers.
pub fn node_object(req: &mut Request, object: PhpObject) -> Option<ffi::Borrowed> {
    let (_, half) = export_row(req, object)?;
    half?(req, object)
}

/// `php_libxml_import_node`: the node an object of any XML extension
/// stands for, through the export the first extension registered for the
/// root of the object's class (`zend_hash_add_ptr` keeps the first), and
/// `None` for an object no export answers.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/></r>');
/// $s = simplexml_import_dom($d->documentElement->firstChild);
/// var_dump($s->getName(), dom_import_simplexml($s)->nodeName);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "a"
/// ```
pub fn import_node(req: &mut Request, object: PhpObject) -> Option<ffi::Exported> {
    let (export, _) = export_row(req, object)?;
    export(req, object)
}

// ---- the natives

/// `libxml_set_streams_context($context): void`: the stream kept for the
/// next load or save, a `stream-context` resource and nothing else.
///
/// ```php
/// libxml_set_streams_context(stream_context_create());
/// try { libxml_set_streams_context(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { libxml_set_streams_context(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { libxml_set_streams_context(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// libxml_set_streams_context(): Argument #1 ($context) must be of type resource, int given
/// libxml_set_streams_context(): Argument #1 ($context) must be of type resource, stdClass given
/// libxml_set_streams_context(): supplied resource is not a valid Stream-Context resource
/// ```
#[php_function]
fn libxml_set_streams_context(req: &mut Request, context: Untyped<Value>) -> Result<(), Thrown> {
    let given = context.0.deref();
    let Some(resource) = given.as_resource() else {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type resource, {name} given"),
        ));
    };
    // `zend_fetch_resource_ex(arg, "Stream-Context", le_stream_context)`.
    StreamCtx::fetch(req, *resource)?;
    let given = Owner::take(req, given);
    if let Some(old) = req.state::<State>().stream_context.replace(given) {
        old.give_up(req);
    }
    Ok(())
}

/// `php_libxml_switch_context`: the `stream_context` every load and
/// save opens its stream with (`LIBXML(stream_context)`) replaced by
/// the one given, and the one it held answered; each owner moves with the
/// value, as soap's `get_sdl` sets the client's around a WSDL fetch and
/// puts the old one back.
pub fn switch_stream_ctx(req: &mut Request, given: Option<Owner<Value>>) -> Option<Owner<Value>> {
    std::mem::replace(&mut req.state::<State>().stream_context, given)
}

/// `libxml_use_internal_errors(?bool $use_errors = null): bool`: the
/// setting before the call, and the list laid down or dropped.
///
/// ```php
/// var_dump(libxml_use_internal_errors(), libxml_use_internal_errors(true), libxml_use_internal_errors(null), libxml_use_internal_errors(false), libxml_use_internal_errors());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn libxml_use_internal_errors(
    req: &mut Request,
    #[php(default = null)] use_errors: Option<bool>,
) -> bool {
    let wanted = use_errors;
    let state = req.state::<State>();
    let before = state.errors.is_some();
    match wanted {
        None => {}
        Some(false) => state.errors = None,
        Some(true) => {
            state.errors.get_or_insert_with(Vec::new);
        }
    }
    before
}

/// `php_libxml_create_error_object`: a `LibXMLError` of one error.
fn error_object(req: &mut Request, error: &Raised) -> Result<Owner<Instance<LibXMLError>>, Thrown> {
    let Some(class) = php_ext::call::lookup_class(req, b"LibXMLError")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: LibXMLError is not declared",
        ));
    };
    let object = php_ext::call::new_object(req, class)?;
    if let Err(thrown) = error_fields(req, *object, error) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(object.into_instance())
}

/// The six properties of a `LibXMLError`, in php's order.
fn error_fields(req: &mut Request, object: PhpObject, error: &Raised) -> Result<(), Thrown> {
    let numbers: [(&[u8], i64); 3] = [
        (b"level", error.level.number()),
        (b"code", i64::from(error.code)),
        (b"column", i64::from(error.column)),
    ];
    for (name, n) in numbers {
        write_prop(req, &object, name, Owner::int(n))?;
    }
    let message = PhpString::new(req, &error.message)?;
    write_prop(req, &object, b"message", message.into_value())?;
    let file = PhpString::new(req, error.file.as_deref().unwrap_or_default())?;
    write_prop(req, &object, b"file", file.into_value())?;
    let line = Owner::int(i64::from(error.line));
    write_prop(req, &object, b"line", line)
}

/// `libxml_get_last_error(): LibXMLError|false`: the list's last with
/// internal errors on, the library's own last otherwise.
///
/// ```php
/// var_dump(libxml_get_last_error());
/// libxml_use_internal_errors(true);
/// var_dump(libxml_get_last_error());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn libxml_get_last_error(
    req: &mut Request,
) -> Result<OrFalse<Owner<Instance<LibXMLError>>>, Thrown> {
    let last = match &req.state::<State>().errors {
        Some(list) => list.last().cloned(),
        None => ffi::Raised::last(),
    };
    match last {
        Some(error) => error_object(req, &error).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `libxml_get_errors(): array`: the list in the order the errors came,
/// empty with internal errors off.
///
/// ```php
/// var_dump(libxml_get_errors());
/// libxml_use_internal_errors(true);
/// var_dump(libxml_get_errors());
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn libxml_get_errors(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let Some(list) = req.state::<State>().errors.clone() else {
        return Ok(Owner::take(req, PhpArray::empty()));
    };
    let mut out = PhpArray::new(req)?;
    for error in &list {
        let object = error_object(req, error)?;
        out.append(req, object.into_value())?;
    }
    Ok(out)
}

/// `libxml_clear_errors(): void`: the library's last error reset and the
/// list emptied.
///
/// ```php
/// libxml_use_internal_errors(true);
/// libxml_clear_errors();
/// var_dump(libxml_get_last_error(), count(libxml_get_errors()));
/// ```
/// ```expect
/// bool(false)
/// int(0)
/// ```
#[php_function]
fn libxml_clear_errors(req: &mut Request) {
    ffi::Raised::reset_last();
    if let Some(list) = req.state::<State>().errors.as_mut() {
        list.clear();
    }
}

/// `libxml_disable_entity_loader(bool $disable = true): bool`,
/// deprecated since 8.0: the setting before the call.
///
/// ```php
/// var_dump(libxml_disable_entity_loader(), libxml_disable_entity_loader(false));
/// ```
/// ```expect
/// Deprecated: Function libxml_disable_entity_loader() is deprecated since 8.0, as external entity loading is disabled by default in %s on line %d
///
/// Deprecated: Function libxml_disable_entity_loader() is deprecated since 8.0, as external entity loading is disabled by default in %s on line %d
/// bool(false)
/// bool(true)
/// ```
#[php_function(deprecated(
    since = "8.0",
    message = "as external entity loading is disabled by default"
))]
fn libxml_disable_entity_loader(req: &mut Request, #[php(default = true)] disable: bool) -> bool {
    let state = req.state::<State>();
    std::mem::replace(&mut state.entity_loader_disabled, disable)
}

/// `libxml_set_external_entity_loader(?callable $resolver_function):
/// true`: the loader kept, or dropped for `null`.
///
/// ```php
/// var_dump(libxml_set_external_entity_loader('strlen'), libxml_get_external_entity_loader());
/// var_dump(libxml_set_external_entity_loader(null), libxml_get_external_entity_loader());
/// ```
/// ```expect
/// bool(true)
/// string(6) "strlen"
/// bool(true)
/// NULL
/// ```
#[php_function]
fn libxml_set_external_entity_loader(
    req: &mut Request,
    resolver_function: Raw<Option<Callable>>,
) -> Result<True, Thrown> {
    let callable = resolver_function.hold(req)?;
    if let Some(old) = req.state::<State>().entity_loader.take() {
        old.give_up(req);
    }
    if let Some(callable) = callable {
        let kept = Owner::take(req, callable);
        req.state::<State>().entity_loader = Some(kept);
    }
    Ok(True)
}

/// `libxml_get_external_entity_loader(): ?callable`: the loader made
/// again from what it resolved to ([`Callable::made_again`],
/// `zend_get_callable_zval_from_fcc`), or `null`.
///
/// ```php
/// $f = fn($p, $s, $c) => null;
/// libxml_set_external_entity_loader($f);
/// var_dump(libxml_get_external_entity_loader() === $f);
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// The loader is kept as `zend_fcc_dup` keeps it, so a `Class::method`
/// string set with `$this` in scope keeps that object, and is answered
/// as the pair it resolved to:
///
/// ```php
/// class A { function __destruct(){ echo "dtor\n"; } function l($p, $s, $c){ return null; } function set(){ libxml_set_external_entity_loader('A::l'); } }
/// (new A)->set();
/// echo "set\n";
/// var_dump(libxml_get_external_entity_loader());
/// libxml_set_external_entity_loader(null);
/// echo "end\n";
/// ```
/// ```expect
/// set
/// array(2) {
///   [0]=>
///   object(A)#1 (0) {
///   }
///   [1]=>
///   string(1) "l"
/// }
/// dtor
/// end
/// ```
#[php_function]
fn libxml_get_external_entity_loader(
    req: &mut Request,
) -> Result<Option<Owner<Raw<Callable>>>, Thrown> {
    let Some(loader) = req.state::<State>().entity_loader.as_deref().cloned() else {
        return Ok(None);
    };
    Ok(Some(loader.made_again(req)?.into_raw()))
}

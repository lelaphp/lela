//! The `tidy` extension: `php-src/ext/tidy` as one crate under `ext/`
//! (ADR-0004), behind the `tidy` feature as php builds it `--with-tidy`,
//! over the keg's libtidy (tidy-html5), the library the oracle links,
//! bound in [`ffi`] alone (design §7.2, revision 72). Its state on the
//! value (ADR-0005). The phpt subset is `php-src/ext/tidy/tests`, through
//! `xtask phpt --subset`.
//!
//! `tidy.c` carried over: the `tidy` and `tidyNode` classes and the
//! `tidy_*` functions ([`doc`], [`node`]), the configuration an array or
//! a file sets, and `ob_tidyhandler` with `tidy.clean_output` over the
//! engine's buffer stack ([`output`]). A document is php's `PHPTidyDoc`,
//! counted by every object that names it, the `tidy` that made it and
//! each `tidyNode` read from it; the count lives in [`State`], since an
//! object's payload names a document and owns none.
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

mod doc;
mod ffi;
mod node;
mod output;
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_const;
use php_ext::decl::{Bytes, IniDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `register_tidy_symbols`: the node types and the tag ids in
/// `tidy.stub.php`'s order, the values of `tidyenum.h`.
///
/// ```php
/// var_dump(TIDY_NODETYPE_ROOT, TIDY_NODETYPE_XMLDECL, TIDY_TAG_UNKNOWN, TIDY_TAG_XMP, TIDY_TAG_ARTICLE, TIDY_TAG_VIDEO);
/// ```
/// ```expect
/// int(0)
/// int(13)
/// int(0)
/// int(121)
/// int(123)
/// int(151)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const TIDY_NODETYPE_ROOT: i64 = 0;
    pub(crate) const TIDY_NODETYPE_DOCTYPE: i64 = 1;
    pub(crate) const TIDY_NODETYPE_COMMENT: i64 = 2;
    pub(crate) const TIDY_NODETYPE_PROCINS: i64 = 3;
    pub(crate) const TIDY_NODETYPE_TEXT: i64 = 4;
    pub(crate) const TIDY_NODETYPE_START: i64 = 5;
    pub(crate) const TIDY_NODETYPE_END: i64 = 6;
    pub(crate) const TIDY_NODETYPE_STARTEND: i64 = 7;
    pub(crate) const TIDY_NODETYPE_CDATA: i64 = 8;
    pub(crate) const TIDY_NODETYPE_SECTION: i64 = 9;
    pub(crate) const TIDY_NODETYPE_ASP: i64 = 10;
    pub(crate) const TIDY_NODETYPE_JSTE: i64 = 11;
    pub(crate) const TIDY_NODETYPE_PHP: i64 = 12;
    pub(crate) const TIDY_NODETYPE_XMLDECL: i64 = 13;
    pub(crate) const TIDY_TAG_UNKNOWN: i64 = 0;
    pub(crate) const TIDY_TAG_A: i64 = 1;
    pub(crate) const TIDY_TAG_ABBR: i64 = 2;
    pub(crate) const TIDY_TAG_ACRONYM: i64 = 3;
    pub(crate) const TIDY_TAG_ADDRESS: i64 = 4;
    pub(crate) const TIDY_TAG_ALIGN: i64 = 5;
    pub(crate) const TIDY_TAG_APPLET: i64 = 6;
    pub(crate) const TIDY_TAG_AREA: i64 = 7;
    pub(crate) const TIDY_TAG_B: i64 = 8;
    pub(crate) const TIDY_TAG_BASE: i64 = 9;
    pub(crate) const TIDY_TAG_BASEFONT: i64 = 10;
    pub(crate) const TIDY_TAG_BDO: i64 = 11;
    pub(crate) const TIDY_TAG_BGSOUND: i64 = 12;
    pub(crate) const TIDY_TAG_BIG: i64 = 13;
    pub(crate) const TIDY_TAG_BLINK: i64 = 14;
    pub(crate) const TIDY_TAG_BLOCKQUOTE: i64 = 15;
    pub(crate) const TIDY_TAG_BODY: i64 = 16;
    pub(crate) const TIDY_TAG_BR: i64 = 17;
    pub(crate) const TIDY_TAG_BUTTON: i64 = 18;
    pub(crate) const TIDY_TAG_CAPTION: i64 = 19;
    pub(crate) const TIDY_TAG_CENTER: i64 = 20;
    pub(crate) const TIDY_TAG_CITE: i64 = 21;
    pub(crate) const TIDY_TAG_CODE: i64 = 22;
    pub(crate) const TIDY_TAG_COL: i64 = 23;
    pub(crate) const TIDY_TAG_COLGROUP: i64 = 24;
    pub(crate) const TIDY_TAG_COMMENT: i64 = 25;
    pub(crate) const TIDY_TAG_DD: i64 = 26;
    pub(crate) const TIDY_TAG_DEL: i64 = 27;
    pub(crate) const TIDY_TAG_DFN: i64 = 28;
    pub(crate) const TIDY_TAG_DIR: i64 = 29;
    pub(crate) const TIDY_TAG_DIV: i64 = 30;
    pub(crate) const TIDY_TAG_DL: i64 = 31;
    pub(crate) const TIDY_TAG_DT: i64 = 32;
    pub(crate) const TIDY_TAG_EM: i64 = 33;
    pub(crate) const TIDY_TAG_EMBED: i64 = 34;
    pub(crate) const TIDY_TAG_FIELDSET: i64 = 35;
    pub(crate) const TIDY_TAG_FONT: i64 = 36;
    pub(crate) const TIDY_TAG_FORM: i64 = 37;
    pub(crate) const TIDY_TAG_FRAME: i64 = 38;
    pub(crate) const TIDY_TAG_FRAMESET: i64 = 39;
    pub(crate) const TIDY_TAG_H1: i64 = 40;
    pub(crate) const TIDY_TAG_H2: i64 = 41;
    pub(crate) const TIDY_TAG_H3: i64 = 42;
    pub(crate) const TIDY_TAG_H4: i64 = 43;
    pub(crate) const TIDY_TAG_H5: i64 = 44;
    pub(crate) const TIDY_TAG_H6: i64 = 45;
    pub(crate) const TIDY_TAG_HEAD: i64 = 46;
    pub(crate) const TIDY_TAG_HR: i64 = 47;
    pub(crate) const TIDY_TAG_HTML: i64 = 48;
    pub(crate) const TIDY_TAG_I: i64 = 49;
    pub(crate) const TIDY_TAG_IFRAME: i64 = 50;
    pub(crate) const TIDY_TAG_ILAYER: i64 = 51;
    pub(crate) const TIDY_TAG_IMG: i64 = 52;
    pub(crate) const TIDY_TAG_INPUT: i64 = 53;
    pub(crate) const TIDY_TAG_INS: i64 = 54;
    pub(crate) const TIDY_TAG_ISINDEX: i64 = 55;
    pub(crate) const TIDY_TAG_KBD: i64 = 56;
    pub(crate) const TIDY_TAG_KEYGEN: i64 = 57;
    pub(crate) const TIDY_TAG_LABEL: i64 = 58;
    pub(crate) const TIDY_TAG_LAYER: i64 = 59;
    pub(crate) const TIDY_TAG_LEGEND: i64 = 60;
    pub(crate) const TIDY_TAG_LI: i64 = 61;
    pub(crate) const TIDY_TAG_LINK: i64 = 62;
    pub(crate) const TIDY_TAG_LISTING: i64 = 63;
    pub(crate) const TIDY_TAG_MAP: i64 = 64;
    pub(crate) const TIDY_TAG_MARQUEE: i64 = 66;
    pub(crate) const TIDY_TAG_MENU: i64 = 67;
    pub(crate) const TIDY_TAG_META: i64 = 68;
    pub(crate) const TIDY_TAG_MULTICOL: i64 = 69;
    pub(crate) const TIDY_TAG_NOBR: i64 = 70;
    pub(crate) const TIDY_TAG_NOEMBED: i64 = 71;
    pub(crate) const TIDY_TAG_NOFRAMES: i64 = 72;
    pub(crate) const TIDY_TAG_NOLAYER: i64 = 73;
    pub(crate) const TIDY_TAG_NOSAVE: i64 = 74;
    pub(crate) const TIDY_TAG_NOSCRIPT: i64 = 75;
    pub(crate) const TIDY_TAG_OBJECT: i64 = 76;
    pub(crate) const TIDY_TAG_OL: i64 = 77;
    pub(crate) const TIDY_TAG_OPTGROUP: i64 = 78;
    pub(crate) const TIDY_TAG_OPTION: i64 = 79;
    pub(crate) const TIDY_TAG_P: i64 = 80;
    pub(crate) const TIDY_TAG_PARAM: i64 = 81;
    pub(crate) const TIDY_TAG_PLAINTEXT: i64 = 83;
    pub(crate) const TIDY_TAG_PRE: i64 = 84;
    pub(crate) const TIDY_TAG_Q: i64 = 85;
    pub(crate) const TIDY_TAG_RB: i64 = 86;
    pub(crate) const TIDY_TAG_RBC: i64 = 87;
    pub(crate) const TIDY_TAG_RP: i64 = 88;
    pub(crate) const TIDY_TAG_RT: i64 = 89;
    pub(crate) const TIDY_TAG_RTC: i64 = 90;
    pub(crate) const TIDY_TAG_RUBY: i64 = 91;
    pub(crate) const TIDY_TAG_S: i64 = 92;
    pub(crate) const TIDY_TAG_SAMP: i64 = 93;
    pub(crate) const TIDY_TAG_SCRIPT: i64 = 94;
    pub(crate) const TIDY_TAG_SELECT: i64 = 95;
    pub(crate) const TIDY_TAG_SERVER: i64 = 96;
    pub(crate) const TIDY_TAG_SERVLET: i64 = 97;
    pub(crate) const TIDY_TAG_SMALL: i64 = 98;
    pub(crate) const TIDY_TAG_SPACER: i64 = 99;
    pub(crate) const TIDY_TAG_SPAN: i64 = 100;
    pub(crate) const TIDY_TAG_STRIKE: i64 = 101;
    pub(crate) const TIDY_TAG_STRONG: i64 = 102;
    pub(crate) const TIDY_TAG_STYLE: i64 = 103;
    pub(crate) const TIDY_TAG_SUB: i64 = 104;
    pub(crate) const TIDY_TAG_SUP: i64 = 105;
    pub(crate) const TIDY_TAG_TABLE: i64 = 107;
    pub(crate) const TIDY_TAG_TBODY: i64 = 108;
    pub(crate) const TIDY_TAG_TD: i64 = 109;
    pub(crate) const TIDY_TAG_TEXTAREA: i64 = 110;
    pub(crate) const TIDY_TAG_TFOOT: i64 = 111;
    pub(crate) const TIDY_TAG_TH: i64 = 112;
    pub(crate) const TIDY_TAG_THEAD: i64 = 113;
    pub(crate) const TIDY_TAG_TITLE: i64 = 114;
    pub(crate) const TIDY_TAG_TR: i64 = 115;
    pub(crate) const TIDY_TAG_TT: i64 = 116;
    pub(crate) const TIDY_TAG_U: i64 = 117;
    pub(crate) const TIDY_TAG_UL: i64 = 118;
    pub(crate) const TIDY_TAG_VAR: i64 = 119;
    pub(crate) const TIDY_TAG_WBR: i64 = 120;
    pub(crate) const TIDY_TAG_XMP: i64 = 121;
    pub(crate) const TIDY_TAG_ARTICLE: i64 = 123;
    pub(crate) const TIDY_TAG_ASIDE: i64 = 124;
    pub(crate) const TIDY_TAG_AUDIO: i64 = 125;
    pub(crate) const TIDY_TAG_BDI: i64 = 126;
    pub(crate) const TIDY_TAG_CANVAS: i64 = 127;
    pub(crate) const TIDY_TAG_COMMAND: i64 = 128;
    pub(crate) const TIDY_TAG_DATALIST: i64 = 130;
    pub(crate) const TIDY_TAG_DETAILS: i64 = 131;
    pub(crate) const TIDY_TAG_DIALOG: i64 = 132;
    pub(crate) const TIDY_TAG_FIGCAPTION: i64 = 133;
    pub(crate) const TIDY_TAG_FIGURE: i64 = 134;
    pub(crate) const TIDY_TAG_FOOTER: i64 = 135;
    pub(crate) const TIDY_TAG_HEADER: i64 = 136;
    pub(crate) const TIDY_TAG_HGROUP: i64 = 137;
    pub(crate) const TIDY_TAG_MAIN: i64 = 138;
    pub(crate) const TIDY_TAG_MARK: i64 = 139;
    pub(crate) const TIDY_TAG_MENUITEM: i64 = 140;
    pub(crate) const TIDY_TAG_METER: i64 = 141;
    pub(crate) const TIDY_TAG_NAV: i64 = 142;
    pub(crate) const TIDY_TAG_OUTPUT: i64 = 143;
    pub(crate) const TIDY_TAG_PROGRESS: i64 = 144;
    pub(crate) const TIDY_TAG_SECTION: i64 = 145;
    pub(crate) const TIDY_TAG_SOURCE: i64 = 146;
    pub(crate) const TIDY_TAG_SUMMARY: i64 = 147;
    pub(crate) const TIDY_TAG_TEMPLATE: i64 = 148;
    pub(crate) const TIDY_TAG_TIME: i64 = 149;
    pub(crate) const TIDY_TAG_TRACK: i64 = 150;
    pub(crate) const TIDY_TAG_VIDEO: i64 = 151;
}

/// `tidy.default_config`, read by every document php makes.
const DEFAULT_CONFIG: &str = "tidy.default_config";
/// `tidy.clean_output`.
const CLEAN_OUTPUT: &str = "tidy.clean_output";

/// A document's place in [`State::docs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DocId(u32);

/// `PHPTidyDoc`: the document, the objects that name it, how many parses
/// were asked of it, whether one was, and php's
/// `emalloc(sizeof(PHPTidyDoc))`, whose size [`ffi::Doc`]'s witness
/// pins.
struct Slot {
    doc: Option<ffi::Doc>,
    refs: u32,
    generation: Generation,
    initialized: bool,
    record: Option<Emalloc>,
}

/// `parse_generation`: which parse of its document a tree was read
/// from. A parse frees the tree before it, so a `tidyNode` read from an
/// earlier one names nodes libtidy no longer holds (8.5.12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Generation(u64);

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. The documents the request's objects name, and
/// `TG(clean_output)`.
#[derive(Default)]
struct State {
    docs: Vec<Option<Slot>>,
    clean_output: bool,
}

/// `sizeof(PHPTidyDoc)`: two pointers, the count, `parse_generation` and
/// `initialized`.
const DOC_RECORD: usize = 40;

/// A new document's slot, counted once, for the object that made it.
pub(crate) fn adopt(req: &mut Request, doc: ffi::Doc) -> Result<DocId, Thrown> {
    let record = Emalloc::new(req, DOC_RECORD)?;
    let slot = Slot {
        doc: Some(doc),
        refs: 1,
        generation: Generation::default(),
        initialized: false,
        record: Some(record),
    };
    let docs = &mut req.state::<State>().docs;
    let at = if let Some(at) = docs.iter().position(Option::is_none) {
        docs[at] = Some(slot);
        at
    } else {
        docs.push(Some(slot));
        docs.len() - 1
    };
    Ok(DocId(u32::try_from(at).unwrap_or(u32::MAX)))
}

fn slot(req: &mut Request, id: DocId) -> Option<&mut Slot> {
    req.state::<State>()
        .docs
        .get_mut(id.0 as usize)
        .and_then(Option::as_mut)
}

/// One more object names the document, `ptdoc->ref_count++`.
pub(crate) fn retain(req: &mut Request, id: DocId) {
    if let Some(slot) = slot(req, id) {
        slot.refs += 1;
    }
}

/// One object fewer names it; the last one released it, as
/// `tidy_object_free_storage` does.
pub(crate) fn forget(req: &mut Request, id: DocId) {
    let Some(slot) = slot(req, id) else {
        return;
    };
    slot.refs = slot.refs.saturating_sub(1);
    if slot.refs > 0 {
        return;
    }
    let (doc, record) = (slot.doc.take(), slot.record.take());
    if let Some(at) = req.state::<State>().docs.get_mut(id.0 as usize) {
        *at = None;
    }
    if let Some(doc) = doc {
        doc.release(req);
    }
    if let Some(record) = record {
        record.free(req);
    }
}

/// Whether a parse was asked of the document, `ptdoc->initialized`.
pub(crate) fn initialized(req: &mut Request, id: DocId) -> bool {
    slot(req, id).is_some_and(|slot| slot.initialized)
}

pub(crate) fn set_initialized(req: &mut Request, id: DocId) {
    if let Some(slot) = slot(req, id) {
        slot.initialized = true;
    }
}

/// The parse the document's tree is from, `ptdoc->parse_generation`;
/// `None` for a document no slot holds.
pub(crate) fn generation(req: &mut Request, id: DocId) -> Option<Generation> {
    slot(req, id).map(|slot| slot.generation)
}

/// One parse more, `ptdoc->parse_generation++`, before libtidy frees
/// the tree it held.
pub(crate) fn reparsed(req: &mut Request, id: DocId) {
    if let Some(slot) = slot(req, id) {
        slot.generation.0 = slot.generation.0.wrapping_add(1);
    }
}

/// `body` over the document, taken out of its slot for the call so the
/// request is free to lend; `None` where the slot holds none.
pub(crate) fn with_doc<R>(
    req: &mut Request,
    id: DocId,
    body: impl FnOnce(&mut Request, &mut ffi::Doc) -> Result<R, Thrown>,
) -> Result<Option<R>, Thrown> {
    let Some(mut doc) = slot(req, id).and_then(|slot| slot.doc.take()) else {
        return Ok(None);
    };
    let answer = body(req, &mut doc);
    match slot(req, id) {
        Some(slot) => slot.doc = Some(doc),
        None => doc.release(req),
    }
    answer.map(Some)
}

/// `php_tidy_load_config`: libtidy's answer read as php words it.
pub(crate) fn load_config(
    req: &mut Request,
    doc: &mut ffi::Doc,
    path: &[u8],
) -> Result<(), Thrown> {
    let answer = doc.load_config(req, path)?;
    let shown = String::from_utf8_lossy(path);
    if answer < 0 {
        let text = format!("Could not load the Tidy configuration file \"{shown}\"");
        php_ext::raising::docref(req, php_ext::raising::Level::Warning, &text);
    } else if answer > 0 {
        let text =
            format!("There were errors while parsing the Tidy configuration file \"{shown}\"");
        php_ext::raising::docref(req, php_ext::raising::Level::Notice, &text);
    }
    Ok(())
}

/// `TIDY_SET_DEFAULT_CONFIG`: `tidy.default_config`'s file, where it
/// names one.
pub(crate) fn default_config(req: &mut Request, doc: &mut ffi::Doc) -> Result<(), Thrown> {
    let path = req
        .ini()
        .get(DEFAULT_CONFIG.as_bytes())
        .unwrap_or_default()
        .to_vec();
    if path.first().is_some_and(|b| *b != 0) {
        load_config(req, doc, &path)?;
    }
    Ok(())
}

/// The extension value (design §7.1): the ini entries, the constants,
/// the natives and the two classes in `tidy.stub.php`'s order, the
/// buffer handler `MINIT` registers, and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('tidy'), in_array('ob_tidyhandler', ob_list_handlers()));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
pub fn extension() -> Extension {
    let builder = ExtensionBuilder::default()
        .ini(vec![
            IniDecl {
                name: Name(Bytes::borrowed(DEFAULT_CONFIG.as_bytes())),
                default: Some(Bytes::borrowed(b"")),
                user: false,
                per_dir: false,
                system: true,
            },
            IniDecl {
                name: Name(Bytes::borrowed(CLEAN_OUTPUT.as_bytes())),
                default: Some(Bytes::borrowed(b"0")),
                user: true,
                per_dir: false,
                system: false,
            },
        ])
        .on_modify(CLEAN_OUTPUT.as_bytes(), output::on_clean_output)
        .constants(constants::DECLARED);
    doc::functions(builder)
        .class::<doc::class::tidy>()
        .class::<node::tidyNode>()
        .register(output::OB_TIDYHANDLER)
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .rinit(output::rinit)
        .info(info)
        .build("tidy")
}

/// `PHP_MINFO_FUNCTION(tidy)`: the table, then `DISPLAY_INI_ENTRIES`,
/// `tidy.clean_output` shown as the boolean its displayer prints.
///
/// ```php
/// $e = new ReflectionExtension('tidy'); ob_start(); $e->info(); echo ob_get_clean();
/// ```
/// ```expect
/// tidy
///
/// Tidy support => enabled
/// libTidy Version => 5.8.0
/// libTidy Release => 2021/07/10
///
/// Directive => Local Value => Master Value
/// tidy.default_config => no value => no value
/// tidy.clean_output => Off => Off
/// ```
fn info(req: &mut Request) {
    let (version, date) = (ffi::version(), ffi::release_date());
    req.extensions_mut().info_table(&[
        &["Tidy support", "enabled"],
        &["libTidy Version", &version],
        &["libTidy Release", &date],
    ]);
    output::info_ini(req);
}

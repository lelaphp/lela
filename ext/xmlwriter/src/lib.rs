//! The `xmlwriter` extension: `php-src/ext/xmlwriter` as one crate under
//! `ext/` (ADR-0004), `php_xmlwriter.c` over libxml2's `xmlTextWriter`,
//! which `ext/libxml` binds and hands out as [`Writer`] (design §4.10,
//! §7.2), with no `src/ffi/` of its own. The phpt subset is
//! `php-src/ext/xmlwriter/tests`, through `xtask phpt --subset`.
//!
//! Every `XMLWriter` carries one payload, [`Intern`], php's
//! `ze_xmlwriter_object`: the emitter, with where its bytes go. Each
//! procedural `xmlwriter_*` function and the method php aliases to it
//! share one body; the emitter is taken off the object for the length of a
//! call, which lends libxml the request so a emitter php's wrappers opened
//! writes through them.
#![forbid(unsafe_code)]
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

mod natives;

use php_ext::abi::{
    Dependency, DependencyKind, DtorObj, Extension, ExtensionBuilder, Instance, ObjectHandlers,
    Path, Refusals, Static, This, Union,
};
use php_ext::attr::php_class;
use php_ext::call::{instantiate, lookup_class, new_object};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw_error};
use php_ext::request::{Request, Resolve};
use php_ext::values::{ClassId, Owner, PhpObject, PhpString};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Emitter, Library, Name, Raised};

/// The extension value (design §7.1).
///
/// ```php
/// var_dump(extension_loaded('xmlwriter'));
/// print_r((new ReflectionExtension('xmlwriter'))->getDependencies());
/// (new ReflectionExtension('xmlwriter'))->info();
/// ```
/// ```expect
/// bool(true)
/// Array
/// (
///     [libxml] => Required
/// )
///
/// xmlwriter
///
/// XMLWriter => enabled
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            natives::xmlwriter_open_uri::declared(),
            natives::xmlwriter_open_memory::declared(),
            natives::xmlwriter_set_indent::declared(),
            natives::xmlwriter_set_indent_string::declared(),
            natives::xmlwriter_start_comment::declared(),
            natives::xmlwriter_end_comment::declared(),
            natives::xmlwriter_start_attribute::declared(),
            natives::xmlwriter_end_attribute::declared(),
            natives::xmlwriter_write_attribute::declared(),
            natives::xmlwriter_start_attribute_ns::declared(),
            natives::xmlwriter_write_attribute_ns::declared(),
            natives::xmlwriter_start_element::declared(),
            natives::xmlwriter_end_element::declared(),
            natives::xmlwriter_full_end_element::declared(),
            natives::xmlwriter_start_element_ns::declared(),
            natives::xmlwriter_write_element::declared(),
            natives::xmlwriter_write_element_ns::declared(),
            natives::xmlwriter_start_pi::declared(),
            natives::xmlwriter_end_pi::declared(),
            natives::xmlwriter_write_pi::declared(),
            natives::xmlwriter_start_cdata::declared(),
            natives::xmlwriter_end_cdata::declared(),
            natives::xmlwriter_write_cdata::declared(),
            natives::xmlwriter_text::declared(),
            natives::xmlwriter_write_raw::declared(),
            natives::xmlwriter_start_document::declared(),
            natives::xmlwriter_end_document::declared(),
            natives::xmlwriter_write_comment::declared(),
            natives::xmlwriter_start_dtd::declared(),
            natives::xmlwriter_end_dtd::declared(),
            natives::xmlwriter_write_dtd::declared(),
            natives::xmlwriter_start_dtd_element::declared(),
            natives::xmlwriter_end_dtd_element::declared(),
            natives::xmlwriter_write_dtd_element::declared(),
            natives::xmlwriter_start_dtd_attlist::declared(),
            natives::xmlwriter_end_dtd_attlist::declared(),
            natives::xmlwriter_write_dtd_attlist::declared(),
            natives::xmlwriter_start_dtd_entity::declared(),
            natives::xmlwriter_end_dtd_entity::declared(),
            natives::xmlwriter_write_dtd_entity::declared(),
            natives::xmlwriter_output_memory::declared(),
            natives::xmlwriter_flush::declared(),
        ])
        .dependencies(vec![Dependency {
            name: String::from("libxml"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .class::<XMLWriter>()
        .info(|req| {
            req.extensions_mut()
                .info_table(&[&["XMLWriter", "enabled"]]);
        })
        .build("xmlwriter")
}

// ---- the object

/// `ze_xmlwriter_object`: the emitter, `None` before one is opened and
/// after the object was destructed.
#[derive(Default)]
pub(crate) struct Intern {
    writer: Option<Emitter>,
}

/// `xmlwriter_object_handlers`: the emitter freed at destruction, which
/// flushes what it holds, and no clone.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }

    /// `xmlwriter_object_dtor`: the emitter freed, which flushes what it
    /// holds, then the class's `__destruct`. A table's own, so an `XMLWriter`
    /// the cycle collector finds is destructor garbage and is flushed
    /// before the collection frees it, as php's `gc_call_destructors`
    /// serves it.
    ///
    /// ```php
    /// class X extends XMLWriter { public $self; }
    /// function x() { $w = new X; $w->openUri('php://output'); $w->startDocument(); $w->writeElement('a', 'b'); $w->self = $w; unset($w); echo "collect\n"; var_dump(gc_collect_cycles()); }
    /// x();
    /// ```
    /// ```expect
    /// collect
    /// <?xml version="1.0"?>
    /// <a>b</a>int(1)
    /// ```
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(|req, object| {
            destroy(req, object);
            DtorObj::Destroy.serve(req, object);
        })
    }
}

/// `XMLWRITER_FROM_OBJECT`: the emitter taken off the object for one call,
/// php's `Error` where it has none.
fn take(req: &mut Request, object: PhpObject) -> Result<Emitter, Thrown> {
    let taken = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.writer.take());
    taken.ok_or_else(|| throw_error(req, "Error", "Invalid or uninitialized XMLWriter object"))
}

/// The emitter put back on the object after a call.
fn put(req: &mut Request, object: PhpObject, writer: Emitter) {
    if let Some(intern) = object.native_mut::<Intern>(req) {
        intern.writer = Some(writer);
    }
}

/// What one call of the emitter answered, as its `xmlTextWriter*` entry
/// answers it.
type Answer = (i32, Vec<Raised>, Option<Thrown>);

type Str = PhpString;
type Opt = Option<PhpString>;

/// One call a native makes of the emitter, as `php_xmlwriter.c`'s natives
/// hand their arguments to an `xmlTextWriter*` entry: the procedural
/// function and the method php aliases to it make the same one. A
/// namespaced call takes the prefix, the name and the URI, in that order.
pub(crate) enum Op {
    SetIndent(bool),
    SetIndentString(Str),
    StartComment,
    EndComment,
    WriteComment(Str),
    StartAttribute(Str),
    EndAttribute,
    WriteAttribute(Str, Str),
    StartAttributeNs(Opt, Str, Opt),
    WriteAttributeNs(Opt, Str, Opt, Str),
    StartElement(Str),
    EndElement,
    FullEndElement,
    StartElementNs(Opt, Str, Opt),
    WriteElement(Str, Opt),
    WriteElementNs(Opt, Str, Opt, Opt),
    StartPi(Str),
    EndPi,
    WritePi(Str, Str),
    StartCdata,
    EndCdata,
    WriteCdata(Str),
    Text(Str),
    WriteRaw(Str),
    /// The version, the encoding and `standalone`.
    StartDocument(Opt, Opt, Opt),
    EndDocument,
    /// The name, then the public and system ids.
    StartDtd(Str, Opt, Opt),
    EndDtd,
    /// The name, the public and system ids, and the subset.
    WriteDtd(Str, Opt, Opt, Opt),
    StartDtdElement(Str),
    EndDtdElement,
    WriteDtdElement(Str, Str),
    StartDtdAttlist(Str),
    EndDtdAttlist,
    WriteDtdAttlist(Str, Str),
    StartDtdEntity(Str, bool),
    EndDtdEntity,
    /// The name, the content, whether it is a parameter entity, and the
    /// public id, system id and notation.
    WriteDtdEntity(Str, Str, bool, Opt, Opt, Opt),
}

impl Op {
    /// `XMLW_NAME_CHK`: the name the call holds to `xmlValidateName`, its
    /// position among the procedural function's arguments (which a method
    /// reports as php does), and what its message calls it.
    fn checked(&self) -> Option<(u32, &'static str, Str)> {
        match *self {
            Op::StartAttribute(n) | Op::WriteAttribute(n, _) | Op::StartDtdEntity(n, _) => {
                Some((2, "attribute name", n))
            }
            Op::StartAttributeNs(_, n, _) | Op::WriteAttributeNs(_, n, _, _) => {
                Some((3, "attribute name", n))
            }
            Op::StartElement(n)
            | Op::WriteElement(n, _)
            | Op::StartDtdElement(n)
            | Op::WriteDtdElement(n, _)
            | Op::StartDtdAttlist(n)
            | Op::WriteDtdAttlist(n, _)
            | Op::WriteDtdEntity(n, ..) => Some((2, "element name", n)),
            Op::StartElementNs(_, n, _) | Op::WriteElementNs(_, n, _, _) => {
                Some((3, "element name", n))
            }
            Op::StartPi(n) | Op::WritePi(n, _) => Some((2, "PI target", n)),
            _ => None,
        }
    }

    /// The `xmlTextWriter*` entry the call is, served; a `write*Element`
    /// with no content is the start, and the end where the start took.
    // The arms name each argument by its letter: name, content, prefix,
    // URI, value, public and system id.
    #[allow(clippy::many_single_char_names)]
    fn run(self, w: &mut Emitter, req: &mut Request) -> Answer {
        match &self {
            Op::SetIndent(on) => w.set_indent(req, *on),
            Op::SetIndentString(s) => w.set_indent_string(req, text(s)),
            Op::StartComment => w.start_comment(req),
            Op::EndComment => w.end_comment(req),
            Op::WriteComment(c) => w.write_comment(req, text(c)),
            Op::StartAttribute(n) => w.start_attribute(req, text(n)),
            Op::EndAttribute => w.end_attribute(req),
            Op::WriteAttribute(n, v) => w.write_attribute(req, text(n), text(v)),
            Op::StartAttributeNs(p, n, u) => {
                w.start_attribute_ns(req, nullable(p), text(n), nullable(u))
            }
            Op::WriteAttributeNs(p, n, u, v) => {
                w.write_attribute_ns(req, nullable(p), text(n), nullable(u), text(v))
            }
            Op::StartElement(n) => w.start_element(req, text(n)),
            Op::EndElement => w.end_element(req),
            Op::FullEndElement => w.full_end_element(req),
            Op::StartElementNs(p, n, u) => {
                w.start_element_ns(req, nullable(p), text(n), nullable(u))
            }
            Op::WriteElement(n, Some(c)) => w.write_element(req, text(n), text(c)),
            Op::WriteElement(n, None) => empty(w, req, Op::StartElement(*n)),
            Op::WriteElementNs(p, n, u, Some(c)) => {
                w.write_element_ns(req, nullable(p), text(n), nullable(u), text(c))
            }
            Op::WriteElementNs(p, n, u, None) => empty(w, req, Op::StartElementNs(*p, *n, *u)),
            Op::StartPi(n) => w.start_pi(req, text(n)),
            Op::EndPi => w.end_pi(req),
            Op::WritePi(n, c) => w.write_pi(req, text(n), text(c)),
            Op::StartCdata => w.start_cdata(req),
            Op::EndCdata => w.end_cdata(req),
            Op::WriteCdata(c) => w.write_cdata(req, text(c)),
            Op::Text(c) => w.write_string(req, text(c)),
            Op::WriteRaw(c) => w.write_raw(req, text(c)),
            Op::StartDocument(v, e, s) => {
                w.start_document(req, nullable(v), nullable(e), nullable(s))
            }
            Op::EndDocument => w.end_document(req),
            Op::StartDtd(n, p, s) => w.start_dtd(req, text(n), nullable(p), nullable(s)),
            Op::EndDtd => w.end_dtd(req),
            Op::WriteDtd(n, p, s, c) => {
                w.write_dtd(req, text(n), nullable(p), nullable(s), nullable(c))
            }
            Op::StartDtdElement(n) => w.start_dtd_element(req, text(n)),
            Op::EndDtdElement => w.end_dtd_element(req),
            Op::WriteDtdElement(n, c) => w.write_dtd_element(req, text(n), text(c)),
            Op::StartDtdAttlist(n) => w.start_dtd_attlist(req, text(n)),
            Op::EndDtdAttlist => w.end_dtd_attlist(req),
            Op::WriteDtdAttlist(n, c) => w.write_dtd_attlist(req, text(n), text(c)),
            Op::StartDtdEntity(n, pe) => w.start_dtd_entity(req, *pe, n.as_bytes()),
            Op::EndDtdEntity => w.end_dtd_entity(req),
            Op::WriteDtdEntity(n, c, pe, p, s, d) => {
                let (n, c) = (n.as_bytes(), c.as_bytes());
                w.write_dtd_entity(req, *pe, n, nullable(p), nullable(s), nullable(d), c)
            }
        }
    }
}

/// A `write*Element` with no content: `start`, then the end where it took.
/// A `string` as the emitter takes it, every string of whose entries is
/// C's nullable pointer.
#[allow(clippy::unnecessary_wraps)]
fn text(s: &Str) -> Option<&[u8]> {
    Some(s.as_bytes())
}

/// A `?string` as the emitter takes it.
fn nullable(s: &Opt) -> Option<&[u8]> {
    s.as_ref().map(PhpString::as_bytes)
}

fn empty(w: &mut Emitter, req: &mut Request, start: Op) -> Answer {
    let (ret, mut raised, thrown) = start.run(w, req);
    if ret == -1 || thrown.is_some() {
        return (ret, raised, thrown);
    }
    let (ret, more, thrown) = w.end_element(req);
    raised.extend(more);
    (ret, raised, thrown)
}

/// What a native is served on: the object a procedural function is handed,
/// or a method's `$this`.
pub(crate) trait Receiver {
    fn receiver(&self) -> PhpObject;
}

impl Receiver for Instance<XMLWriter> {
    fn receiver(&self) -> PhpObject {
        self.object()
    }
}

impl Receiver for This<XMLWriter> {
    fn receiver(&self) -> PhpObject {
        self.object()
    }
}

/// One call of the emitter on `object`: `XMLWRITER_FROM_OBJECT`, the
/// name `XMLW_NAME_CHK` holds, the call served, what libxml raised
/// routed, and `true` where libxml did not answer `-1`.
pub(crate) fn call(req: &mut Request, on: &impl Receiver, op: Op) -> Result<bool, Thrown> {
    let object = on.receiver();
    let mut writer = take(req, object)?;
    if let Some((position, subject, name)) =
        op.checked().filter(|(_, _, n)| !Name::valid(n.as_bytes()))
    {
        put(req, object, writer);
        let name = name.as_bytes();
        let shown = String::from_utf8_lossy(php_ext::coerce::c_str(name));
        let tail = format!("be a valid {subject}, \"{shown}\" given");
        return Err(arg_value_error(req, position, &tail));
    }
    let (ret, raised, thrown) = op.run(&mut writer, req);
    put(req, object, writer);
    Errors::settle(req, raised, thrown)?;
    Ok(ret != -1)
}

/// `_xmlwriter_get_valid_file_path`: the path a emitter opens `source`
/// at, a local path made absolute and its directory required to exist;
/// `None` where it cannot be resolved.
fn valid_file_path(req: &mut Request, source: &[u8]) -> Option<Vec<u8>> {
    let mut local = source;
    if Library::escaped_has_scheme(source) {
        let lower = source.to_ascii_lowercase();
        let skip = if lower.starts_with(b"file:///") {
            7
        } else if lower.starts_with(b"file://localhost/") {
            16
        } else {
            return Some(source.to_vec());
        };
        // Nothing past the host's slash is no file.
        if source.len() == skip + 1 {
            return None;
        }
        local = &source[skip..];
    }
    let resolved = if let Some(path) = req.environment_mut().realpath(local) {
        path.to_string_lossy().into_owned().into_bytes()
    } else {
        // `expand_filepath`.
        let cwd = req
            .environment()
            .cwd()
            .into_os_string()
            .into_encoded_bytes();
        req.environment_mut()
            .virtual_file_ex(&cwd, local, Resolve::FilePath)?
    };
    // `zend_dirname` of the path as written, and `php_sys_stat` of it.
    if let Some(at) = local.iter().rposition(|b| *b == b'/') {
        let dir = if at == 0 { &b"/"[..] } else { &local[..at] };
        if !req.environment().absolutize(dir).exists() {
            return None;
        }
    }
    Some(resolved)
}

/// What opening a URI came to.
pub(crate) enum Opened {
    /// `_xmlwriter_get_valid_file_path` refused the path.
    Unresolved,
    /// libxml made no emitter over it.
    Failed,
    Emitter(Emitter),
}

/// `xmlNewTextWriterFilename` over the resolved path of `source`.
pub(crate) fn open_uri(req: &mut Request, source: &[u8]) -> Result<Opened, Thrown> {
    let Some(path) = valid_file_path(req, source) else {
        return Ok(Opened::Unresolved);
    };
    let (writer, raised, thrown) = Emitter::uri(req, &path);
    Errors::settle(req, raised, thrown)?;
    Ok(writer.map_or(Opened::Failed, Opened::Emitter))
}

/// [`open_uri`] as `xmlwriter_open_uri` and `openUri` refuse: an empty
/// path is php's `ValueError`, one that resolves to nothing its warning;
/// `None` beside either and where libxml made no emitter.
pub(crate) fn open_warned(req: &mut Request, uri: Path) -> Result<Option<Emitter>, Thrown> {
    if uri.0.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    match open_uri(req, uri.0.as_bytes())? {
        Opened::Unresolved => {
            docref(req, Level::Warning, "Unable to resolve file path");
            Ok(None)
        }
        Opened::Failed => Ok(None),
        Opened::Emitter(writer) => Ok(Some(writer)),
    }
}

/// `xmlwriter_class_entry_ce`.
fn writer_class(req: &mut Request) -> Result<ClassId, Thrown> {
    let class = lookup_class(req, b"XMLWriter")?;
    class.ok_or_else(|| fatal(req, "InternalError: XMLWriter is not linked"))
}

/// `xmlwriter_object_new` with the emitter on it, for the two procedural
/// constructors.
pub(crate) fn new_writer(
    req: &mut Request,
    writer: Emitter,
) -> Result<Owner<Instance<XMLWriter>>, Thrown> {
    let class = writer_class(req)?;
    let object = new_object(req, class)?;
    put(req, *object, writer);
    Ok(object.into_instance())
}

/// `xml_writer_create_static`: `object_init_with_constructor` of the
/// class the call named, the emitter on it.
pub(crate) fn create_static(req: &mut Request, writer: Emitter) -> Result<Owner<Static>, Thrown> {
    let scope = req.called_scope();
    let class = scope.map_or_else(|| writer_class(req), Ok)?;
    match instantiate(req, class, &[]) {
        Ok(object) => {
            put(req, *object, writer);
            Ok(object.into_static())
        }
        Err(thrown) => {
            let (raised, _thrown) = writer.close(req);
            Errors::route(req, raised);
            Err(thrown)
        }
    }
}

/// The object's emitter replaced, the old one freed first
/// (`xmlwriter_destroy_libxml_objects`).
pub(crate) fn replace(req: &mut Request, object: PhpObject, writer: Emitter) {
    destroy(req, object);
    put(req, object, writer);
}

/// `xmlwriter_destroy_libxml_objects`: the emitter freed, which flushes
/// what it holds to where it writes.
fn destroy(req: &mut Request, object: PhpObject) {
    if let Some(old) = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.writer.take())
    {
        let (raised, _thrown) = old.close(req);
        Errors::route(req, raised);
    }
}

/// `php_xmlwriter_flush`: the memory target's bytes, emptied where `empty`
/// asks; for any other target the bytes `xmlTextWriterFlush` wrote, or
/// with `force_string` the empty string.
pub(crate) fn flush(
    req: &mut Request,
    object: PhpObject,
    empty: bool,
    force_string: bool,
) -> Result<Union<Owner<PhpString>, i64>, Thrown> {
    let mut writer = take(req, object)?;
    if force_string && writer.buffered().is_none() {
        put(req, object, writer);
        return PhpString::new(req, b"").map(Union::A);
    }
    let (written, raised, thrown) = writer.flush(req);
    let bytes = match writer.buffered() {
        None => None,
        Some(_) if empty => Some(writer.take_buffered()),
        Some(held) => Some(held.to_vec()),
    };
    put(req, object, writer);
    Errors::settle(req, raised, thrown)?;
    match bytes {
        Some(b) => PhpString::new(req, &b).map(Union::A),
        None => Ok(Union::B(i64::from(written))),
    }
}

/// [`flush`] as `xmlwriter_output_memory` answers it, a string always.
pub(crate) fn output_memory(
    req: &mut Request,
    object: PhpObject,
    empty: bool,
) -> Result<Owner<PhpString>, Thrown> {
    match flush(req, object, empty, true)? {
        Union::A(s) => Ok(s),
        Union::B(_) => PhpString::new(req, b""),
    }
}

/// `class XMLWriter`: one `ze_xmlwriter_object` each, uncloneable; its
/// methods are in [`natives`], beside the functions they alias.
///
/// ```php
/// $w = new XMLWriter; var_dump($w->openMemory(), $w->setIndent(false), $w->setIndentString(' '));
/// $w->startDocument('1.0', 'UTF-8'); $w->startDtd('r'); $w->writeDtdElement('r', 'ANY'); $w->startDtdElement('s'); $w->text('EMPTY'); $w->endDtdElement();
/// $w->writeDtdAttlist('r', 'a CDATA #IMPLIED'); $w->startDtdAttlist('s'); $w->endDtdAttlist(); $w->writeDtdEntity('e', 'v'); $w->startDtdEntity('f', false); $w->endDtdEntity(); $w->endDtd();
/// $w->writeDtd('x'); $w->startComment(); $w->text('c'); $w->endComment(); $w->writeComment('d');
/// $w->startPi('t'); $w->endPi(); $w->writePi('u', 'v');
/// $w->startElement('r'); $w->startAttribute('a'); $w->text('1'); $w->endAttribute(); $w->writeAttribute('b', '2');
/// $w->startAttributeNs('p', 'c', 'urn:p'); $w->endAttribute(); $w->writeAttributeNs('q', 'd', 'urn:q', '4');
/// $w->startElementNs('p', 'e', 'urn:p'); $w->fullEndElement(); $w->writeElement('f'); $w->writeElementNs('q', 'g', 'urn:q', 'h');
/// $w->startCdata(); $w->text('x'); $w->endCdata(); $w->writeCdata('y'); $w->writeRaw('<z/>'); $w->endElement(); $w->endDocument();
/// var_dump(strlen($w->flush(false)) > 0); echo $w->outputMemory();
/// try { clone $w; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { (new XMLWriter)->text('x'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $w->startElement('a b'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// <?xml version="1.0" encoding="UTF-8"?>
/// <!DOCTYPE r [<!ELEMENT r ANY><!ELEMENT s EMPTY><!ATTLIST r a CDATA #IMPLIED><!ATTLIST s><!ENTITY e "v"><!ENTITY f>]><!DOCTYPE x><!--c--><!--d--><?t?><?u v?><r a="1" b="2" p:c="" q:d="4" xmlns:q="urn:q" xmlns:p="urn:p"><p:e xmlns:p="urn:p"></p:e><f/><q:g xmlns:q="urn:q">h</q:g><![CDATA[x]]><![CDATA[y]]><z/></r>
/// Trying to clone an uncloneable object of class XMLWriter
/// Invalid or uninitialized XMLWriter object
/// XMLWriter::startElement(): Argument #2 must be a valid element name, "a b" given
/// ```
#[php_class(size = 72, payload = Intern, handlers = HANDLERS)]
pub(crate) struct XMLWriter;

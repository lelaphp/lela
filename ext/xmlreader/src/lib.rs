//! The `xmlreader` extension: `php-src/ext/xmlreader` as one crate under
//! `ext/` (ADR-0004), `php_xmlreader.c` over libxml2's `xmlTextReader`,
//! which `ext/libxml` binds and hands out as [`TextReader`] (design §4.10,
//! §7.2), with no `src/ffi/` of its own. The phpt subset is
//! `php-src/ext/xmlreader/tests`, through `xtask phpt --subset`.
//!
//! Every `XMLReader` carries one payload, [`Intern`], php's
//! `xmlreader_object`. Its fourteen properties are the table
//! `xmlreader_prop_handlers` registers, [`Property`], each read off the
//! reader as it stands; `open` and `XML` called on an object reach the
//! object through the trampolines `xmlreader_get_method` answers
//! ([`trampoline`]), and `expand` answers its node through dom's
//! `php_dom_create_object`.
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

mod throwables;
mod trampoline;

use php_ext::abi::{
    DebugInfo, Dependency, DependencyKind, Extension, ExtensionBuilder, Instance, ObjectHandlers,
    OrFalse, Path, PropertyPlace, PropertyRead, PropertyTest, Refusals, Static, Std, This, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{Function, instantiate, lookup_class};
use php_ext::coerce::truthy;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, fatal, throw_error, type_error};
use php_ext::request::{Request, Resolve, Stream};
use php_ext::values::walk;
use php_ext::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Resource, Value, ValueRef,
};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Expanded, Library, Raised, RelaxNg, TextReader};

use throwables::Throwables;

/// The extension value (design §7.1).
///
/// ```php
/// var_dump(extension_loaded('xmlreader'));
/// print_r((new ReflectionExtension('xmlreader'))->getDependencies());
/// (new ReflectionExtension('xmlreader'))->info();
/// ```
/// ```expect
/// bool(true)
/// Array
/// (
///     [dom] => Required
///     [libxml] => Required
/// )
///
/// xmlreader
///
/// XMLReader => enabled
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .dependencies(vec![dependency("dom"), dependency("libxml")])
        .class::<XMLReader>()
        .class::<trampoline::XMLReader>()
        .engine_state(Throwables::start)
        .info(|req| {
            req.extensions_mut()
                .info_table(&[&["XMLReader", "enabled"]]);
        })
        .build("xmlreader")
}

fn dependency(name: &str) -> Dependency {
    Dependency {
        name: String::from(name),
        kind: DependencyKind::Required,
        rel: None,
        version: None,
    }
}

/// `dom_node_class_entry`, named for `expand`'s parameter.
#[php_class(extern)]
pub(crate) struct DOMNode;

// ---- the object

/// `xmlreader_object`: the reader with the memory input and the schema it
/// keeps beside it, `None` before one is loaded and after `close`.
#[derive(Default)]
pub(crate) struct Intern {
    reader: Option<TextReader>,
}

/// `xmlreader_object_handlers`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// What the table refuses: cloning.
const REFUSALS: Refusals = Refusals::NONE.cloning();

impl ObjectHandlers for Handlers {
    fn refusals(&self) -> Refusals {
        REFUSALS
    }

    /// `xmlreader_objects_free_storage`: `zend_object_std_dtor`, then
    /// the reader's resources.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        free_resources(req, object);
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        match Property::of(name) {
            Some(p) => Ok(match p.read(req, object)? {
                Some(value) => value,
                None => Owner::null(),
            }),
            None => Std.read_property(req, object, name, read),
        }
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if Property::of(name.as_bytes()).is_none() {
            return Std.write_property(req, object, name, value);
        }
        let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
        name.give_up(req);
        value.give_up(req);
        let class = class_name(req, object.class());
        let message = format!("Cannot modify readonly property {class}::${shown}");
        let ce = Throwables::of(req)?;
        Err(throw_error(req, ce.error, &message))
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        match Property::of(name) {
            Some(_) => Ok(PropertyPlace::Handlers),
            None => Std.get_property_ptr_ptr(req, object, name),
        }
    }

    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(p) = Property::of(name) else {
            return Std.has_property(req, object, name, test);
        };
        if test == PropertyTest::Exists {
            return Ok(true);
        }
        let Some(value) = p.read(req, object)? else {
            return Ok(false);
        };
        let answer = match test {
            PropertyTest::NotEmpty => truthy(req, &value),
            _ => Ok(!matches!(value.as_ref(), ValueRef::Null)),
        };
        value.give_up(req);
        answer
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if Property::of(name).is_none() {
            return Std.unset_property(req, object, name);
        }
        let class = class_name(req, object.class());
        let message = format!("Cannot unset {class}::${}", String::from_utf8_lossy(name));
        let ce = Throwables::of(req)?;
        Err(throw_error(req, ce.error, &message))
    }

    /// `xmlreader_get_debug_info`: the object's own properties, then each
    /// handled one that reads, the throwable of one that fails left
    /// pending beside the table.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        // `zend_array_dup(zend_std_get_properties(object))`.
        let own = object.property_table(req)?;
        let own: Vec<(Key, Value)> = walk::rows(req, own).collect();
        let mut thrown: Option<Thrown> = None;
        let table = PhpArray::filled(req, |req, table| {
            for (key, value) in own {
                table.set(req, Owner::take(req, key), Owner::take(req, value))?;
            }
            for (name, p) in Property::ALL {
                match p.read(req, object) {
                    Ok(Some(value)) => {
                        // The table's row takes the value's owner.
                        table.set_str(req, name, value)?;
                    }
                    Ok(None) => {}
                    Err(t) => {
                        thrown = Some(match thrown.take() {
                            Some(earlier) => earlier.chain(req, t),
                            None => t,
                        });
                    }
                }
            }
            Ok(())
        })?;
        Ok(Some(DebugInfo { table, thrown }))
    }

    /// `xmlreader_get_method`: `open` and `XML`, the two static natives
    /// php lays a non-static copy of beside the table, answered as that
    /// copy, so called on an object they load into it.
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        let found = Std.get_method(req, object, lowered)?;
        let native = found
            .and_then(|f| req.symbols().function_decl(f))
            .is_some_and(|decl| decl.file.0.is_empty());
        if native && (lowered == b"open" || lowered == b"xml") {
            let class = reader_class(req)?;
            return Ok(req.symbols().trampoline(class, lowered).or(found));
        }
        Ok(found)
    }
}

fn class_name(req: &Request, class: ClassId) -> String {
    req.symbols()
        .class_decl(class)
        .map(|d| String::from_utf8_lossy(&d.name.0).into_owned())
        .unwrap_or_default()
}

/// `xmlreader_class_entry`.
fn reader_class(req: &mut Request) -> Result<ClassId, Thrown> {
    match lookup_class(req, b"XMLReader")? {
        Some(class) => Ok(class),
        None => Err(fatal(req, "InternalError: XMLReader is not linked")),
    }
}

/// `xmlreader_free_resources`: the reader, its input and its schema freed
/// with the request lent to what closing them reaches.
fn free_resources(req: &mut Request, object: PhpObject) {
    if let Some(reader) = object
        .native_mut::<Intern>(req)
        .and_then(|i| i.reader.take())
    {
        let raised = reader.close(req);
        Errors::route(req, raised);
    }
}

/// The reader taken off the object for one call, `None` where it has none.
fn take(req: &mut Request, object: PhpObject) -> Option<TextReader> {
    object
        .native_mut::<Intern>(req)
        .and_then(|i| i.reader.take())
}

/// The reader put back on the object after a call.
fn put(req: &mut Request, object: PhpObject, reader: TextReader) {
    if let Some(intern) = object.native_mut::<Intern>(req) {
        intern.reader = Some(reader);
    }
}

/// `f` on the object's reader, `None` where it has none; what libxml
/// raised routed and what a stream threw handed on.
fn with<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut TextReader, &mut Request) -> (R, Vec<Raised>, Option<Thrown>),
) -> Result<Option<R>, Thrown> {
    let Some(mut reader) = take(req, object) else {
        return Ok(None);
    };
    let (answer, raised, thrown) = f(&mut reader, req);
    put(req, object, reader);
    Errors::settle(req, raised, thrown)?;
    Ok(Some(answer))
}

/// A call that raises nothing on the object's reader.
fn quiet<R>(
    req: &mut Request,
    object: PhpObject,
    f: impl FnOnce(&mut TextReader) -> R,
) -> Option<R> {
    object
        .native_mut::<Intern>(req)
        .and_then(|i| i.reader.as_mut())
        .map(f)
}

/// `php_xmlreader_string_arg`'s and the attribute getters' answer: the
/// string libxml handed back as [`fstring`] makes it, `NULL` where it
/// handed back none.
fn string_or_null(
    req: &mut Request,
    bytes: Option<Vec<u8>>,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    bytes.map(|b| fstring(req, &b)).transpose()
}

/// `php_xmlreader_no_arg_string`'s answer: the string libxml handed
/// back as [`fstring`] makes it, `RETVAL_EMPTY_STRING()` where it handed
/// back none.
fn string_or_empty(req: &mut Request, bytes: Option<Vec<u8>>) -> Result<Owner<PhpString>, Thrown> {
    match bytes {
        Some(bytes) => fstring(req, &bytes),
        None => PhpString::new(req, b""),
    }
}

/// `ZVAL_FSTRING`, which every string libxml hands the reader back
/// through since php 8.5.12 (`Z/Zend/zend_API.h:1001`): the bytes copied
/// into a `zend_string_finit` asked for fallibly, so a string past the
/// memory limit is refused rather than served, the answer is the
/// interned empty string `ZVAL_FSTRINGL` leaves in its place, and the
/// soft limit's fatal is raised at the next safepoint, never the hard
/// limit's.
fn fstring(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    match req
        .memory_mut()
        .fallible(|req| PhpString::finit(req, bytes))?
    {
        Some(made) => Ok(made),
        None => PhpString::new(req, b""),
    }
}

/// `zend_argument_must_not_be_empty_error`.
fn not_empty(req: &mut Request, position: u32, value: &[u8]) -> Result<(), Thrown> {
    if value.is_empty() {
        return Err(arg_value_error(req, position, "not be empty"));
    }
    Ok(())
}

/// `xmlreader_valid_encoding`.
fn valid_encoding(req: &mut Request, encoding: Option<&Path>) -> Result<(), Thrown> {
    match encoding {
        Some(e) if !Library::known_encoding(e.0.as_bytes()) => {
            Err(arg_value_error(req, 2, "be a valid character encoding"))
        }
        _ => Ok(()),
    }
}

/// `_xmlreader_get_valid_file_path`: a local path made absolute, anything
/// with a scheme but `file` as it is; `None` where a local path resolves
/// to nothing.
fn valid_file_path(req: &mut Request, source: &[u8]) -> Option<Vec<u8>> {
    let mut local = source;
    if Library::escaped_has_scheme(source) {
        let lower = source.to_ascii_lowercase();
        if lower.starts_with(b"file:///") {
            local = &source[7..];
        } else if lower.starts_with(b"file://localhost/") {
            local = &source[16..];
        } else {
            return Some(source.to_vec());
        }
    }
    if let Some(path) = req.environment_mut().realpath(local) {
        return Some(path.to_string_lossy().into_owned().into_bytes());
    }
    let cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    req.environment_mut()
        .virtual_file_ex(&cwd, local, Resolve::FilePath)
}

/// Whether an object's own `open` or `XML` refuses as php's warning or a
/// static factory as php's `Error`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Warn,
    Throw,
}

/// Where a load leaves its reader: on the object it was called on, or on
/// a new object of a class.
#[derive(Clone, Copy)]
pub(crate) enum Target {
    This(PhpObject),
    New(ClassId),
}

impl Target {
    /// The loaded reader on the object, or a new object with it.
    fn land(self, req: &mut Request, reader: TextReader) -> Result<Owner<Value>, Thrown> {
        match self {
            Target::This(object) => {
                put(req, object, reader);
                Ok(Owner::bool(true))
            }
            Target::New(class) => match instantiate(req, class, &[], &[]) {
                Ok(object) => {
                    put(req, *object, reader);
                    Ok(object.into_value())
                }
                Err(thrown) => {
                    let raised = reader.close(req);
                    Errors::route(req, raised);
                    Err(thrown)
                }
            },
        }
    }
}

/// `Unable to open source data` or its kin, as the call refuses.
fn refuse(req: &mut Request, failure: Failure, message: &str) -> Result<Owner<Value>, Thrown> {
    match failure {
        Failure::Warn => {
            docref(req, Level::Warning, message);
            Ok(Owner::bool(false))
        }
        Failure::Throw => {
            let ce = Throwables::of(req)?;
            Err(throw_error(req, ce.error, message))
        }
    }
}

/// `xml_reader_from_uri`.
pub(crate) fn from_uri(
    req: &mut Request,
    into: Target,
    failure: Failure,
    uri: Path,
    encoding: Option<&Path>,
    flags: i64,
) -> Result<Owner<Value>, Thrown> {
    if let Target::This(object) = into {
        free_resources(req, object);
    }
    not_empty(req, 1, uri.0.as_bytes())?;
    valid_encoding(req, encoding)?;
    let mut reader = None;
    if let Some(path) = valid_file_path(req, uri.0.as_bytes()) {
        let e = encoding.map(|e| e.0.as_bytes());
        let (made, raised, thrown) = TextReader::file(req, &path, e, flags_int(flags));
        Errors::settle(req, raised, thrown)?;
        reader = made;
    }
    match reader {
        Some(reader) => into.land(req, reader),
        None => refuse(req, failure, "Unable to open source data"),
    }
}

/// `xml_reader_from_string`.
pub(crate) fn from_string(
    req: &mut Request,
    into: Target,
    failure: Failure,
    source: PhpString,
    encoding: Option<&Path>,
    flags: i64,
) -> Result<Owner<Value>, Thrown> {
    if let Target::This(object) = into {
        free_resources(req, object);
    }
    not_empty(req, 1, source.as_bytes())?;
    valid_encoding(req, encoding)?;
    // The script's directory as the document's base, `xmlCanonicPath` of
    // the working directory with its slash.
    let mut cwd = req
        .environment()
        .cwd()
        .into_os_string()
        .into_encoded_bytes();
    if cwd.last() != Some(&b'/') {
        cwd.push(b'/');
    }
    let base = Library::canonic_path(&cwd);
    let e = encoding.map(|e| e.0.as_bytes());
    let (made, raised, thrown) =
        TextReader::memory(req, source.as_bytes(), base.as_deref(), e, flags_int(flags));
    Errors::settle(req, raised, thrown)?;
    match made {
        Some(reader) => into.land(req, reader),
        None => refuse(req, failure, "Unable to load source data"),
    }
}

/// A `zend_long` handed to libxml's `int`, truncated as C truncates it.
#[allow(clippy::cast_possible_truncation)]
fn flags_int(flags: i64) -> i32 {
    flags as i32
}

/// The class a static load of `XMLReader` makes: `open` and `XML` always
/// `XMLReader`, the `from*` factories the class they were called on.
fn called(req: &mut Request) -> Result<ClassId, Thrown> {
    match req.frames().called_scope() {
        Some(class) => Ok(class),
        None => reader_class(req),
    }
}

// ---- the properties

/// One of the fourteen names `xmlreader_prop_handlers` serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Property {
    AttributeCount,
    BaseUri,
    Depth,
    HasAttributes,
    HasValue,
    IsDefault,
    IsEmptyElement,
    LocalName,
    Name,
    NamespaceUri,
    NodeType,
    Prefix,
    Value,
    XmlLang,
}

impl Property {
    /// The table in `PHP_MINIT_FUNCTION(xmlreader)`'s order, which the
    /// debug table keeps.
    const ALL: [(&'static [u8], Property); 14] = [
        (b"attributeCount", Property::AttributeCount),
        (b"baseURI", Property::BaseUri),
        (b"depth", Property::Depth),
        (b"hasAttributes", Property::HasAttributes),
        (b"hasValue", Property::HasValue),
        (b"isDefault", Property::IsDefault),
        (b"isEmptyElement", Property::IsEmptyElement),
        (b"localName", Property::LocalName),
        (b"name", Property::Name),
        (b"namespaceURI", Property::NamespaceUri),
        (b"nodeType", Property::NodeType),
        (b"prefix", Property::Prefix),
        (b"value", Property::Value),
        (b"xmlLang", Property::XmlLang),
    ];

    fn of(name: &[u8]) -> Option<Property> {
        Property::ALL
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, p)| *p)
    }

    /// `xmlreader_property_reader`: the value off the reader as it
    /// stands, the empty one of the type where the object has none; a
    /// number libxml answers `-1` for is php's `Error`. A string is
    /// `ZVAL_FSTRING`'s, asked for fallibly ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-value.xml'; $h = fopen($f, 'w'); fwrite($h, '<r a="');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '"/>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read(); $r->moveToAttribute('a');
    /// var_dump($r->value, 'unreached');
    /// ```
    /// ```expect exit=255
    /// string(0) ""
    /// string(9) "unreached"
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    fn read(self, req: &mut Request, object: PhpObject) -> Result<Option<Owner<Value>>, Thrown> {
        let text =
            |f: fn(&TextReader) -> Option<Vec<u8>>| move |r: &mut TextReader| Read::Text(f(r));
        let number = |f: fn(&mut TextReader) -> i32, boolean: bool| {
            move |r: &mut TextReader| Read::Number(f(r), boolean)
        };
        let read = match self {
            Property::AttributeCount => {
                quiet(req, object, number(TextReader::attribute_count, false))
            }
            Property::Depth => quiet(req, object, number(TextReader::depth, false)),
            Property::NodeType => quiet(req, object, number(TextReader::node_type, false)),
            Property::HasAttributes => quiet(req, object, number(TextReader::has_attributes, true)),
            Property::HasValue => quiet(req, object, number(TextReader::has_value, true)),
            Property::IsDefault => quiet(req, object, number(TextReader::is_default, true)),
            Property::IsEmptyElement => {
                quiet(req, object, number(TextReader::is_empty_element, true))
            }
            Property::BaseUri => quiet(req, object, text(TextReader::base_uri)),
            Property::LocalName => quiet(req, object, text(TextReader::local_name)),
            Property::Name => quiet(req, object, text(TextReader::name)),
            Property::NamespaceUri => quiet(req, object, text(TextReader::namespace_uri)),
            Property::Prefix => quiet(req, object, text(TextReader::prefix)),
            Property::Value => quiet(req, object, text(TextReader::value)),
            Property::XmlLang => quiet(req, object, text(TextReader::xml_lang)),
        };
        let boolean = matches!(
            self,
            Property::HasAttributes
                | Property::HasValue
                | Property::IsDefault
                | Property::IsEmptyElement
        );
        match read {
            Some(Read::Number(-1, _)) => {
                let ce = Throwables::of(req)?;
                Err(throw_error(
                    req,
                    ce.error,
                    "Failed to read property because no XML data has been read yet",
                ))
            }
            Some(Read::Number(n, true)) => Ok(Some(Owner::bool(n != 0))),
            Some(Read::Number(n, false)) => Ok(Some(Owner::int(i64::from(n)))),
            Some(Read::Text(t)) => Ok(Some(string_or_empty(req, t)?.into_value())),
            None if boolean => Ok(Some(Owner::bool(false))),
            None => match self {
                Property::AttributeCount | Property::Depth | Property::NodeType => {
                    Ok(Some(Owner::int(0)))
                }
                _ => Ok(Some(PhpString::new(req, b"")?.into_value())),
            },
        }
    }
}

/// What one property read off the reader: a number, and whether it is
/// a `bool`, or a string.
enum Read {
    Number(i32, bool),
    Text(Option<Vec<u8>>),
}

// ---- the class

/// `class XMLReader`.
///
/// ```php
/// $r = new XMLReader; var_dump($r->XML('<r xmlns:p="urn:p" xml:lang="en"><p:a x="1" y="2">t</p:a><!--c--><b/></r>'));
/// while ($r->read()) { echo $r->nodeType, ' ', $r->name, ' ', $r->depth, ' ', $r->localName, ' ', $r->prefix, ' ', $r->namespaceURI, ' ', $r->value, ' ', $r->xmlLang, ' ', (int) $r->isEmptyElement, (int) $r->hasValue, (int) $r->hasAttributes, ' ', $r->attributeCount, "\n"; }
/// $r = XMLReader::XML('<r a="1"/>'); $r->read();
/// var_dump($r->getAttribute('a'), $r->getAttributeNo(0), $r->getAttributeNo(5), $r->moveToAttribute('a'), $r->name, $r->moveToElement(), $r->name);
/// var_dump($r->moveToFirstAttribute(), $r->moveToNextAttribute(), $r->isValid(), $r->lookupNamespace('xml'), $r->readString(), $r->readInnerXml(), $r->readOuterXml());
/// var_dump($r->getParserProperty(XMLReader::LOADDTD), $r->setParserProperty(XMLReader::SUBST_ENTITIES, true), $r->close());
/// try { $r->read(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $r->getAttribute(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { clone $r; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// 1 r 0 r    en 001 2
/// 1 p:a 1 a p urn:p  en 001 2
/// 3 #text 2 #text   t en 010 0
/// 15 p:a 1 a p urn:p  en 001 0
/// 8 #comment 1 #comment   c en 010 0
/// 1 b 1 b    en 100 0
/// 15 r 0 r    en 001 0
/// string(1) "1"
/// string(1) "1"
/// NULL
/// bool(true)
/// string(1) "a"
/// bool(true)
/// string(1) "r"
/// bool(true)
/// bool(false)
/// bool(false)
/// string(36) "http://www.w3.org/XML/1998/namespace"
/// string(0) ""
/// string(0) ""
/// string(10) "<r a="1"/>"
/// bool(false)
/// bool(true)
/// bool(true)
/// Data must be loaded before reading
/// XMLReader::getAttribute(): Argument #1 ($name) must not be empty
/// Trying to clone an uncloneable object of class XMLReader
/// ```
#[php_class(size = 80, payload = Intern, handlers = HANDLERS)]
pub(crate) struct XMLReader;

#[php_class]
impl XMLReader {
    const NONE: i64 = 0;
    const ELEMENT: i64 = 1;
    const ATTRIBUTE: i64 = 2;
    const TEXT: i64 = 3;
    const CDATA: i64 = 4;
    const ENTITY_REF: i64 = 5;
    const ENTITY: i64 = 6;
    const PI: i64 = 7;
    const COMMENT: i64 = 8;
    const DOC: i64 = 9;
    const DOC_TYPE: i64 = 10;
    const DOC_FRAGMENT: i64 = 11;
    const NOTATION: i64 = 12;
    const WHITESPACE: i64 = 13;
    const SIGNIFICANT_WHITESPACE: i64 = 14;
    const END_ELEMENT: i64 = 15;
    const END_ENTITY: i64 = 16;
    const XML_DECLARATION: i64 = 17;
    const LOADDTD: i64 = 1;
    const DEFAULTATTRS: i64 = 2;
    const VALIDATE: i64 = 3;
    const SUBST_ENTITIES: i64 = 4;

    #[php(property, virtual)]
    const attributeCount: i64;
    #[php(property, virtual)]
    const baseURI: PhpString;
    #[php(property, virtual)]
    const depth: i64;
    #[php(property, virtual)]
    const hasAttributes: bool;
    #[php(property, virtual)]
    const hasValue: bool;
    #[php(property, virtual)]
    const isDefault: bool;
    #[php(property, virtual)]
    const isEmptyElement: bool;
    #[php(property, virtual)]
    const localName: PhpString;
    #[php(property, virtual)]
    const name: PhpString;
    #[php(property, virtual)]
    const namespaceURI: PhpString;
    #[php(property, virtual)]
    const nodeType: i64;
    #[php(property, virtual)]
    const prefix: PhpString;
    #[php(property, virtual)]
    const value: PhpString;
    #[php(property, virtual)]
    const xmlLang: PhpString;

    #[php(tentative)]
    fn close(req: &mut Request, this: This<Self>) -> php_ext::abi::True {
        free_resources(req, this.object());
        php_ext::abi::True
    }

    /// `getAttribute(string $name): ?string`, the value asked for
    /// fallibly as php 8.5.12's `RETVAL_FSTRING` asks ([`fstring`]): one
    /// past the memory limit ends the request at the soft limit's fatal.
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-getattribute.xml'; $h = fopen($f, 'w'); fwrite($h, '<r a="');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '"/>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->getAttribute('a'), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn getAttribute(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        not_empty(req, 1, name.as_bytes())?;
        let found = quiet(req, this.object(), |r| r.attribute(name.as_bytes())).flatten();
        string_or_null(req, found)
    }

    /// `getAttributeNo(int $index): ?string`, the value asked for
    /// fallibly ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-getattributeno.xml'; $h = fopen($f, 'w'); fwrite($h, '<r a="');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '"/>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->getAttributeNo(0), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn getAttributeNo(
        req: &mut Request,
        this: This<Self>,
        index: i64,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let found = quiet(req, this.object(), |r| r.attribute_no(flags_int(index))).flatten();
        string_or_null(req, found)
    }

    /// `getAttributeNs(string $name, string $namespace): ?string`.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r xmlns:p="urn:p" p:a="1"/>'); $r->read();
    /// var_dump($r->getAttributeNs('a', 'urn:p'), $r->getAttributeNs('a', 'urn:q'));
    /// try { $r->getAttributeNs('a', ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "1"
    /// NULL
    /// XMLReader::getAttributeNs(): Argument #2 ($namespace) must not be empty
    /// ```
    ///
    /// The value is asked for fallibly ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-getattributens.xml'; $h = fopen($f, 'w'); fwrite($h, '<r xmlns:p="urn:p" p:a="');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '"/>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->getAttributeNs('a', 'urn:p'), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn getAttributeNs(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        namespace: PhpString,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        not_empty(req, 1, name.as_bytes())?;
        not_empty(req, 2, namespace.as_bytes())?;
        let found = quiet(req, this.object(), |r| {
            r.attribute_ns(name.as_bytes(), namespace.as_bytes())
        });
        string_or_null(req, found.flatten())
    }

    #[php(tentative)]
    fn getParserProperty(
        req: &mut Request,
        this: This<Self>,
        property: i64,
    ) -> Result<bool, Thrown> {
        match quiet(req, this.object(), |r| r.parser_prop(flags_int(property))) {
            None => {
                let ce = Throwables::of(req)?;
                Err(throw_error(
                    req,
                    ce.error,
                    "Cannot access parser properties before loading data",
                ))
            }
            Some(-1) => Err(arg_value_error(req, 1, "be a valid parser property")),
            Some(n) => Ok(n != 0),
        }
    }

    #[php(tentative)]
    fn isValid(req: &mut Request, this: This<Self>) -> bool {
        quiet(req, this.object(), TextReader::is_valid) == Some(1)
    }

    /// `lookupNamespace(string $prefix): ?string`, the namespace asked
    /// for fallibly ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-lookupnamespace.xml'; $h = fopen($f, 'w'); fwrite($h, '<r xmlns:p="');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '"/>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->lookupNamespace('p'), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn lookupNamespace(
        req: &mut Request,
        this: This<Self>,
        prefix: PhpString,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        not_empty(req, 1, prefix.as_bytes())?;
        let found = quiet(req, this.object(), |r| {
            r.lookup_namespace(prefix.as_bytes())
        })
        .flatten();
        string_or_null(req, found)
    }

    #[php(tentative)]
    fn moveToAttribute(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<bool, Thrown> {
        not_empty(req, 1, name.as_bytes())?;
        Ok(quiet(req, this.object(), |r| r.move_to_attribute(name.as_bytes())) == Some(1))
    }

    /// `moveToAttributeNo(int $index): bool`.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r a="1" b="2"/>'); $r->read();
    /// var_dump($r->moveToAttributeNo(1), $r->name, $r->moveToAttributeNo(2), (new XMLReader)->moveToAttributeNo(0));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "b"
    /// bool(false)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn moveToAttributeNo(req: &mut Request, this: This<Self>, index: i64) -> bool {
        quiet(req, this.object(), |r| {
            r.move_to_attribute_no(flags_int(index))
        }) == Some(1)
    }

    /// `moveToAttributeNs(string $name, string $namespace): bool`.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r xmlns:p="urn:p" p:a="1"/>'); $r->read();
    /// var_dump($r->moveToAttributeNs('a', 'urn:p'), $r->name, $r->value, $r->moveToAttributeNs('b', 'urn:p'));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(3) "p:a"
    /// string(1) "1"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn moveToAttributeNs(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        namespace: PhpString,
    ) -> Result<bool, Thrown> {
        not_empty(req, 1, name.as_bytes())?;
        not_empty(req, 2, namespace.as_bytes())?;
        let moved = quiet(req, this.object(), |r| {
            r.move_to_attribute_ns(name.as_bytes(), namespace.as_bytes())
        });
        Ok(moved == Some(1))
    }

    #[php(tentative)]
    fn moveToElement(req: &mut Request, this: This<Self>) -> bool {
        quiet(req, this.object(), TextReader::move_to_element) == Some(1)
    }

    #[php(tentative)]
    fn moveToFirstAttribute(req: &mut Request, this: This<Self>) -> bool {
        quiet(req, this.object(), TextReader::move_to_first_attribute) == Some(1)
    }

    #[php(tentative)]
    fn moveToNextAttribute(req: &mut Request, this: This<Self>) -> bool {
        quiet(req, this.object(), TextReader::move_to_next_attribute) == Some(1)
    }

    #[php(tentative)]
    fn read(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        match with(req, this.object(), TextReader::read)? {
            None => {
                let ce = Throwables::of(req)?;
                Err(throw_error(
                    req,
                    ce.error,
                    "Data must be loaded before reading",
                ))
            }
            Some(n) => Ok(n == 1),
        }
    }

    /// `next(?string $name = null): bool`: the next sibling, or the next of that local name.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r><a/><b><c/></b><a/><d/></r>'); $r->read(); $r->read();
    /// var_dump($r->next(), $r->name, $r->next('d'), $r->name, $r->next('zz'));
    /// try { (new XMLReader)->next(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// string(1) "b"
    /// bool(true)
    /// string(1) "d"
    /// bool(false)
    /// Data must be loaded before reading
    /// ```
    #[php(tentative)]
    fn next(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] name: Option<PhpString>,
    ) -> Result<bool, Thrown> {
        let object = this.object();
        let Some(mut n) = with(req, object, TextReader::next)? else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Data must be loaded before reading",
            ));
        };
        if let Some(name) = name {
            let wanted = php_ext::coerce::c_str(name.as_bytes());
            while n == 1 {
                if quiet(req, object, |r| r.local_name()).flatten().as_deref() == Some(wanted) {
                    return Ok(true);
                }
                n = with(req, object, TextReader::next)?.unwrap_or(-1);
            }
        }
        Ok(n == 1)
    }

    fn open(
        req: &mut Request,
        uri: Path,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let class = reader_class(req)?;
        from_uri(
            req,
            Target::New(class),
            Failure::Warn,
            uri,
            encoding.as_ref(),
            flags,
        )
        .map(Untyped)
    }

    /// `fromUri(string $uri, ?string $encoding = null, int $flags = 0): static`.
    ///
    /// ```php
    /// $f = sys_get_temp_dir() . '/xmlreader_fromuri.xml'; file_put_contents($f, '<b/>');
    /// class R extends XMLReader {} $r = R::fromUri($f); var_dump(get_class($r), $r->read(), $r->name); unlink($f);
    /// try { XMLReader::fromUri('/nonexistent/x.xml'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { XMLReader::fromUri('x.xml', 'no-such-encoding'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "R"
    /// bool(true)
    /// string(1) "b"
    /// Unable to open source data
    /// XMLReader::fromUri(): Argument #2 ($encoding) must be a valid character encoding
    /// ```
    fn fromUri(
        req: &mut Request,
        uri: Path,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<Static>, Thrown> {
        let class = called(req)?;
        let made = from_uri(
            req,
            Target::New(class),
            Failure::Throw,
            uri,
            encoding.as_ref(),
            flags,
        )?;
        static_of(req, made)
    }

    /// `fromStream($stream, ?string $encoding = null, int $flags = 0, ?string $documentUri = null): static`: a reader over a stream it never closes.
    ///
    /// ```php
    /// $s = fopen('php://memory', 'w+'); fwrite($s, '<r><a>t</a></r>'); rewind($s);
    /// $r = XMLReader::fromStream($s, null, 0, 'http://example.com/'); $seen = []; while ($r->read()) $seen[] = $r->nodeType . $r->name; echo implode(' ', $seen), "\n"; var_dump($r->baseURI, is_resource($s));
    /// $c = fopen('php://memory', 'r'); fclose($c); try { XMLReader::fromStream($c); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $g = fopen('php://memory', 'w+'); fwrite($g, '<a/>'); rewind($g); $kept = XMLReader::fromStream($g); unset($g); var_dump($kept->read(), $kept->name);
    /// ```
    /// ```expect
    /// 1r 1a 3#text 15a 15r
    /// string(0) ""
    /// bool(true)
    /// XMLReader::fromStream(): supplied resource is not a valid stream resource
    /// bool(true)
    /// string(1) "a"
    /// ```
    fn fromStream(
        req: &mut Request,
        stream: Resource,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
        #[php(default = null)] documentUri: Option<Path>,
    ) -> Result<Owner<Static>, Thrown> {
        let Some(stream) = Stream::from_value(req, &Value::resource(stream)) else {
            let message =
                "XMLReader::fromStream(): supplied resource is not a valid stream resource";
            return Err(type_error(req, message));
        };
        valid_encoding(req, encoding.as_ref())?;
        let (e, u) = (
            encoding.as_ref().map(|e| e.0.as_bytes()),
            documentUri.as_ref().map(|u| u.0.as_bytes()),
        );
        let (made, raised, thrown) = TextReader::stream(req, stream, u, e, flags_int(flags));
        Errors::settle(req, raised, thrown)?;
        let Some(reader) = made else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Could not construct libxml reader",
            ));
        };
        let class = called(req)?;
        let made = Target::New(class).land(req, reader)?;
        static_of(req, made)
    }

    /// `readInnerXml(): string`, the markup asked for fallibly
    /// ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-readinnerxml.xml'; $h = fopen($f, 'w'); fwrite($h, '<r><a>');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '</a></r>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->readInnerXml(), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn readInnerXml(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let read = with(req, this.object(), TextReader::read_inner_xml)?.flatten();
        string_or_empty(req, read)
    }

    /// `readOuterXml(): string`, the markup asked for fallibly
    /// ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-readouterxml.xml'; $h = fopen($f, 'w'); fwrite($h, '<r><a>');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '</a></r>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->readOuterXml(), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn readOuterXml(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let read = with(req, this.object(), TextReader::read_outer_xml)?.flatten();
        string_or_empty(req, read)
    }

    /// `readString(): string`, the text asked for fallibly
    /// ([`fstring`]):
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// $f = sys_get_temp_dir() . '/xmlreader-oom-readstring.xml'; $h = fopen($f, 'w'); fwrite($h, '<r>');
    /// for ($i = 0; $i < 6; $i++) fwrite($h, str_repeat('x', 512 * 1024));
    /// fwrite($h, '</r>'); fclose($h); register_shutdown_function(fn() => unlink($f));
    /// $r = XMLReader::open($f); $r->read();
    /// var_dump($r->readString(), 'unreached');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 3162112 bytes) in %SCRATCH%/witness.php on line 7
    /// ```
    #[php(tentative)]
    fn readString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let read = with(req, this.object(), TextReader::read_string)?.flatten();
        string_or_empty(req, read)
    }

    /// `setSchema(?string $filename): bool`: W3C XSD validation of what is read, set before the first read.
    ///
    /// ```php
    /// $x = sys_get_temp_dir() . '/xmlreader_schema.xsd';
    /// file_put_contents($x, '<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:element name="r" type="xs:int"/></xs:schema>');
    /// $r = XMLReader::fromString('<r>1</r>'); var_dump($r->setSchema($x)); while ($r->read()); var_dump($r->isValid()); unlink($x);
    /// try { (new XMLReader)->setSchema(null); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// Schema must be set prior to reading
    /// ```
    #[php(tentative)]
    fn setSchema(
        req: &mut Request,
        this: This<Self>,
        filename: Option<Path>,
    ) -> Result<bool, Thrown> {
        if let Some(f) = &filename {
            not_empty(req, 1, f.0.as_bytes())?;
        }
        let xsd = filename.as_ref().map(|f| f.0.as_bytes());
        match with(req, this.object(), |r, req| r.schema_validate(req, xsd))? {
            None => {
                let ce = Throwables::of(req)?;
                Err(throw_error(
                    req,
                    ce.error,
                    "Schema must be set prior to reading",
                ))
            }
            Some(0) => Ok(true),
            Some(_) => {
                docref(req, Level::Warning, "Schema contains errors");
                Ok(false)
            }
        }
    }

    #[php(tentative)]
    fn setParserProperty(
        req: &mut Request,
        this: This<Self>,
        property: i64,
        value: bool,
    ) -> Result<bool, Thrown> {
        match quiet(req, this.object(), |r| {
            r.set_parser_prop(flags_int(property), value)
        }) {
            None => {
                let ce = Throwables::of(req)?;
                Err(throw_error(
                    req,
                    ce.error,
                    "Cannot access parser properties before loading data",
                ))
            }
            Some(-1) => Err(arg_value_error(req, 1, "be a valid parser property")),
            Some(_) => Ok(true),
        }
    }

    /// `setRelaxNGSchema(?string $filename): bool`.
    ///
    /// ```php
    /// $x = sys_get_temp_dir() . '/xmlreader_rng.rng';
    /// file_put_contents($x, '<element name="r" xmlns="http://relaxng.org/ns/structure/1.0"><text/></element>');
    /// $r = XMLReader::fromString('<r>t</r>'); var_dump($r->setRelaxNGSchema($x)); while ($r->read()); var_dump($r->isValid()); unlink($x);
    /// $r = XMLReader::fromString('<r/>'); var_dump(@$r->setRelaxNGSchema('/nonexistent.rng'), $r->setRelaxNGSchema(null));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    #[php(tentative)]
    fn setRelaxNGSchema(
        req: &mut Request,
        this: This<Self>,
        filename: Option<Path>,
    ) -> Result<bool, Thrown> {
        relaxng(req, this.object(), filename.as_ref(), true)
    }

    /// `setRelaxNGSchemaSource(?string $source): bool`.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r>t</r>');
    /// var_dump($r->setRelaxNGSchemaSource('<element name="r" xmlns="http://relaxng.org/ns/structure/1.0"><text/></element>')); while ($r->read()); var_dump($r->isValid());
    /// try { $r->setRelaxNGSchemaSource(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// XMLReader::setRelaxNGSchemaSource(): Argument #1 ($source) must not be empty
    /// ```
    #[php(tentative)]
    fn setRelaxNGSchemaSource(
        req: &mut Request,
        this: This<Self>,
        source: Option<Path>,
    ) -> Result<bool, Thrown> {
        relaxng(req, this.object(), source.as_ref(), false)
    }

    fn XML(
        req: &mut Request,
        source: PhpString,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let class = reader_class(req)?;
        from_string(
            req,
            Target::New(class),
            Failure::Warn,
            source,
            encoding.as_ref(),
            flags,
        )
        .map(Untyped)
    }

    /// `fromString(string $source, ?string $encoding = null, int $flags = 0): static`.
    ///
    /// ```php
    /// class R extends XMLReader { function __construct() { echo "made\n"; } }
    /// $r = R::fromString('<a/>'); var_dump(get_class($r), $r->read(), $r->name);
    /// try { XMLReader::fromString(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// made
    /// string(1) "R"
    /// bool(true)
    /// string(1) "a"
    /// XMLReader::fromString(): Argument #1 ($source) must not be empty
    /// ```
    fn fromString(
        req: &mut Request,
        source: PhpString,
        #[php(default = null)] encoding: Option<Path>,
        #[php(default = 0)] flags: i64,
    ) -> Result<Owner<Static>, Thrown> {
        let class = called(req)?;
        let made = from_string(
            req,
            Target::New(class),
            Failure::Throw,
            source,
            encoding.as_ref(),
            flags,
        )?;
        static_of(req, made)
    }

    /// `expand(?DOMNode $baseNode = null): DOMNode|false`: a copy of the current node and its subtree, in `$baseNode`'s document where one is given.
    ///
    /// ```php
    /// $r = XMLReader::fromString('<r><a>t</a></r>'); $r->read(); $r->read();
    /// $n = $r->expand(); var_dump($n->nodeName, $n->textContent);
    /// var_dump(@$r->expand(new DOMElement('x')) instanceof DOMNode);
    /// try { (new XMLReader)->expand(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "a"
    /// string(1) "t"
    /// bool(true)
    /// Data must be loaded before expanding
    /// ```
    #[php(tentative)]
    fn expand(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] baseNode: Option<Instance<DOMNode>>,
    ) -> Result<OrFalse<Owner<Instance<DOMNode>>>, Thrown> {
        let base = baseNode.map(|b| b.object());
        let mut doc = None;
        if let Some(object) = base {
            let Some(node) = php_ext_libxml::import_node(req, object).map(|e| e.node()) else {
                let class = class_name(req, object.class());
                docref(req, Level::Warning, &format!("Couldn't fetch {class}"));
                return Ok(OrFalse::False);
            };
            doc = node.doc();
        }
        let Some(expanded) = with(req, this.object(), |r, req| r.expand(req, doc))? else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Data must be loaded before expanding",
            ));
        };
        match expanded {
            Expanded::Failed => {
                docref(req, Level::Warning, "An Error Occurred while expanding");
                Ok(OrFalse::False)
            }
            Expanded::Uncopyable => {
                docref(req, Level::Notice, "Cannot expand this node type");
                Ok(OrFalse::False)
            }
            Expanded::Copy(node) => {
                let made = php_ext_dom::create_object(req, node, base)?;
                Ok(match made.into_object() {
                    Ok(object) => OrFalse::Value(object.into_instance()),
                    Err(other) => {
                        other.give_up(req);
                        OrFalse::False
                    }
                })
            }
        }
    }
}

/// A new object a factory made, as `static`.
fn static_of(req: &mut Request, made: Owner<Value>) -> Result<Owner<Static>, Thrown> {
    match made.into_object() {
        Ok(object) => Ok(object.into_static()),
        Err(other) => {
            other.give_up(req);
            Err(fatal(
                req,
                "InternalError: XMLReader factory made no object",
            ))
        }
    }
}

/// `php_xmlreader_set_relaxng_schema`.
fn relaxng(
    req: &mut Request,
    object: PhpObject,
    source: Option<&Path>,
    file: bool,
) -> Result<bool, Thrown> {
    if let Some(s) = source {
        not_empty(req, 1, s.0.as_bytes())?;
    }
    let resolved = match source {
        Some(s) if file => valid_file_path(req, s.0.as_bytes()).map(Some),
        Some(s) => Some(Some(s.0.as_bytes().to_vec())),
        None => Some(None),
    };
    let answer = with(req, object, |r, req| match &resolved {
        None => (-1, Vec::new(), None),
        Some(None) => r.relaxng(req, None),
        Some(Some(bytes)) if file => r.relaxng(req, Some(RelaxNg::File(bytes))),
        Some(Some(bytes)) => r.relaxng(req, Some(RelaxNg::Source(bytes))),
    })?;
    match answer {
        None => {
            let ce = Throwables::of(req)?;
            Err(throw_error(
                req,
                ce.error,
                "Schema must be set prior to reading",
            ))
        }
        Some(0) => Ok(true),
        Some(_) => {
            docref(req, Level::Warning, "Schema contains errors");
            Ok(false)
        }
    }
}

//! `DOMDocument` (`document.c`): the document object, its parse and save
//! switches, the node factories, loading and saving through libxml2, and
//! validation.

use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi as f;
use php_ext_libxml::ffi::{
    DocString, Kind, Name, Node, Ns, Parsed, Parser, Route, Save, Schema, Source, Validate,
    Validated,
};

use crate::map::MapKind;
use crate::object::Code;
use crate::props::Prop;
use crate::{
    Entry, Read, adopt, arity, bool_arg, bytes_arg, check_qname, dom, dom_error, dom_mut, fetch,
    file_path, get_ns, get_ns_unchecked, instance_of, invalidate, long_arg, nullable_bytes_arg,
    nullable_object, object_arg, object_for, path_arg, props_of, strict, string, this, this_node,
    validate_and_extract,
};

/// `XML_XML_NAMESPACE`.
pub(crate) const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";
/// `DOM_XMLNS_NS_URI`.
pub(crate) const XMLNS_NS: &[u8] = b"http://www.w3.org/2000/xmlns/";

/// `zend_argument_must_not_be_empty_error`.
fn must_not_be_empty(req: &mut Request, position: u32) -> Thrown {
    arg_value_error(req, position, "not be empty")
}

/// Every error of `raised` routed to php's generic handler, as a validator
/// meets them.
fn generic(mut raised: Vec<php_ext_libxml::ffi::Raised>) -> Vec<php_ext_libxml::ffi::Raised> {
    for error in &mut raised {
        error.route = Route::Generic;
    }
    raised
}

// ---- the virtual properties

fn doc_node(req: &mut Request, object: PhpObject) -> Result<Node, Thrown> {
    crate::prop_node(req, object)
}

/// `dom_document_doctype_read`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->doctype, $d->documentElement, get_class($d->implementation));
/// $d->loadXML('<!DOCTYPE r><r/>');
/// var_dump($d->doctype->name, $d->documentElement->tagName);
/// ```
/// ```expect
/// NULL
/// NULL
/// string(17) "DOMImplementation"
/// string(1) "r"
/// string(1) "r"
/// ```
fn doctype(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(nullable_object(req, doc.int_subset(), Some(object))?))
}

fn implementation(req: &mut Request, _object: PhpObject) -> Read {
    let class = crate::class(req, "DOMImplementation")?;
    Ok(Some(php_ext::call::new_object(req, class)?.as_value()))
}

fn document_element(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(nullable_object(req, doc.root(), Some(object))?))
}

/// `dom_document_encoding_read`.
///
/// ```php
/// $d = new DOMDocument('1.0', 'UTF-8');
/// var_dump($d->encoding, $d->xmlEncoding, $d->version, $d->standalone, $d->documentURI);
/// echo $d->saveXML();
/// ```
/// ```expect
/// string(5) "UTF-8"
/// string(5) "UTF-8"
/// string(3) "1.0"
/// bool(false)
/// NULL
/// <?xml version="1.0" encoding="UTF-8"?>
/// ```
fn encoding(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(crate::nullable_string(
        req,
        doc.doc_encoding().as_deref(),
    )?))
}

fn actual_encoding(req: &mut Request, object: PhpObject) -> Read {
    crate::deprecated_property(req, "Property DOMDocument::$actualEncoding is deprecated");
    encoding(req, object)
}

fn set_encoding(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    match value.as_str_bytes() {
        Some(name) if php_ext_libxml::ffi::Library::known_encoding(name) => {
            doc.set_doc_string(DocString::Encoding, Some(name));
            Ok(())
        }
        _ => Err(throw(req, "ValueError", "Invalid document encoding")),
    }
}

fn standalone(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(Value::bool(doc.doc_standalone() > 0)))
}

fn set_standalone(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    doc.set_doc_standalone(i32::from(value.as_bool().unwrap_or(false)));
    Ok(())
}

fn version(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(crate::nullable_string(
        req,
        doc.doc_version().as_deref(),
    )?))
}

fn set_version(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    doc.set_doc_string(
        DocString::Version,
        Some(value.as_str_bytes().unwrap_or_default()),
    );
    Ok(())
}

fn document_uri(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(crate::nullable_string(req, doc.doc_url().as_deref())?))
}

fn set_document_uri(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    doc.set_doc_string(
        DocString::Url,
        Some(value.as_str_bytes().unwrap_or_default()),
    );
    Ok(())
}

#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn config(req: &mut Request, _object: PhpObject) -> Read {
    crate::deprecated_property(req, "Property DOMDocument::$config is deprecated");
    Ok(Some(Value::null()))
}

/// One of `libxml_doc_props`' switches, read and written.
#[derive(Clone, Copy)]
enum Switch {
    Strict,
    Format,
    Validate,
    Resolve,
    Preserve,
    Recover,
    Substitute,
}

#[allow(clippy::unnecessary_wraps)] // the reader shape every row of a property table has
fn read_switch(req: &mut Request, object: PhpObject, which: Switch) -> Read {
    let p = props_of(req, object);
    Ok(Some(Value::bool(match which {
        Switch::Strict => p.strict_error,
        Switch::Format => p.format,
        Switch::Validate => p.validate_on_parse,
        Switch::Resolve => p.resolve_externals,
        Switch::Preserve => p.preserve_whitespace,
        Switch::Recover => p.recover,
        Switch::Substitute => p.substitute_entities,
    })))
}

#[allow(clippy::unnecessary_wraps)] // the writer shape every row of a property table has
fn write_switch(
    req: &mut Request,
    object: PhpObject,
    value: &Value,
    which: Switch,
) -> Result<(), Thrown> {
    let on = value.as_bool().unwrap_or(false);
    if let Some(p) = dom_mut(req, object).and_then(|d| d.obj.props_mut()) {
        match which {
            Switch::Strict => p.strict_error = on,
            Switch::Format => p.format = on,
            Switch::Validate => p.validate_on_parse = on,
            Switch::Resolve => p.resolve_externals = on,
            Switch::Preserve => p.preserve_whitespace = on,
            Switch::Recover => p.recover = on,
            Switch::Substitute => p.substitute_entities = on,
        }
    }
    Ok(())
}

macro_rules! switch {
    ($read:ident, $write:ident, $which:ident) => {
        fn $read(req: &mut Request, object: PhpObject) -> Read {
            read_switch(req, object, Switch::$which)
        }
        fn $write(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
            write_switch(req, object, value, Switch::$which)
        }
    };
}

switch!(strict_read, strict_write, Strict);
switch!(format_read, format_write, Format);
switch!(validate_read, validate_write, Validate);
switch!(resolve_read, resolve_write, Resolve);
switch!(preserve_read, preserve_write, Preserve);
switch!(recover_read, recover_write, Recover);
switch!(substitute_read, substitute_write, Substitute);

const fn ro(name: &'static [u8], read: fn(&mut Request, PhpObject) -> Read) -> Prop {
    Prop {
        name,
        read,
        write: None,
    }
}

const fn rw(
    name: &'static [u8],
    read: fn(&mut Request, PhpObject) -> Read,
    write: fn(&mut Request, PhpObject, &Value) -> Result<(), Thrown>,
) -> Prop {
    Prop {
        name,
        read,
        write: Some(write),
    }
}

/// `dom_document_prop_handlers`, before the node's merged behind them.
pub(crate) const PROPS: &[Prop] = &[
    ro(b"doctype", doctype),
    ro(b"implementation", implementation),
    ro(b"documentElement", document_element),
    ro(b"actualEncoding", actual_encoding),
    rw(b"encoding", encoding, set_encoding),
    ro(b"xmlEncoding", encoding),
    rw(b"standalone", standalone, set_standalone),
    rw(b"xmlStandalone", standalone, set_standalone),
    rw(b"version", version, set_version),
    rw(b"xmlVersion", version, set_version),
    rw(b"strictErrorChecking", strict_read, strict_write),
    rw(b"documentURI", document_uri, set_document_uri),
    ro(b"config", config),
    rw(b"formatOutput", format_read, format_write),
    rw(b"validateOnParse", validate_read, validate_write),
    rw(b"resolveExternals", resolve_read, resolve_write),
    rw(b"preserveWhiteSpace", preserve_read, preserve_write),
    rw(b"recover", recover_read, recover_write),
    rw(b"substituteEntities", substitute_read, substitute_write),
];

// ---- construction and loading

/// `DOMDocument::__construct(string $version = "1.0", string $encoding = "")`.
///
/// ```php
/// $d = new DOMDocument('1.1', 'ISO-8859-1');
/// echo $d->saveXML();
/// $d->__construct();
/// echo $d->saveXML();
/// ```
/// ```expect
/// <?xml version="1.1" encoding="ISO-8859-1"?>
/// <?xml version="1.0"?>
/// ```
fn construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 2)?;
    let version = match crate::arg(req, 1)? {
        Some(v) => v.as_str_bytes().map(<[u8]>::to_vec),
        None => Some(b"1.0".to_vec()),
    };
    let encoding = bytes_arg(req, 2)?;
    let this = this(req)?;
    let Some(doc) = Node::new_doc(version.as_deref()) else {
        return Err(crate::dom_throw(req, Code::InvalidState));
    };
    if !encoding.is_empty() {
        doc.set_doc_string(DocString::Encoding, Some(&encoding));
    }
    if let Some(d) = dom_mut(req, this) {
        d.obj.set_object(this);
        d.obj.release();
        d.obj.adopt_document(doc);
        d.obj.take_node(doc);
    }
    Ok(Value::null())
}

/// `php_dom_finish_loading_document`: the object takes the new tree, the
/// old one let go of, the switches and the class map carried over.
fn finish_loading(req: &mut Request, this: PhpObject, doc: Option<Node>) -> Value {
    let Some(doc) = doc else {
        return Value::bool(false);
    };
    if let Some(d) = dom_mut(req, this) {
        d.obj.set_object(this);
        let mut class_type = php_ext_libxml::ffi::ClassType::Legacy;
        let mut modification = 0;
        let mut props = None;
        if d.obj.is_bound() {
            class_type = d.obj.class_type();
            modification = d.obj.modification();
            d.obj.decrement_node_ptr();
            props = d.obj.take_props();
            d.obj.decrement_doc_ref();
        }
        d.obj.adopt_document(doc);
        d.obj.put_props(props);
        d.obj.set_class_type(class_type);
        d.obj.take_node(doc);
        if modification != 0 {
            d.obj.set_modification(modification);
            d.obj.invalidate();
        }
    }
    Value::bool(true)
}

/// The directory a document parsed from memory takes as its base, php's
/// `VCWD_GETCWD` with its trailing slash.
fn base_directory(req: &Request) -> Vec<u8> {
    let mut dir = req.cwd().0.to_string_lossy().into_owned().into_bytes();
    if !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    dir
}

/// `dom_parse_document`.
fn parse_document(req: &mut Request, from_file: bool) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let source = bytes_arg(req, 1)?;
    let options = long_arg(req, 2, 0)?;
    let this = this(req)?;
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let Ok(options) = i32::try_from(options) else {
        docref(req, Level::Warning, "Invalid options");
        return Ok(Value::bool(false));
    };
    let props = props_of(req, this);
    let mut options = options;
    if props.validate_on_parse {
        options |= f::XML_PARSE_DTDVALID;
    }
    if props.resolve_externals {
        options |= f::XML_PARSE_DTDATTR;
    }
    if props.substitute_entities {
        options |= f::XML_PARSE_NOENT;
    }
    if !props.preserve_whitespace {
        options |= f::XML_PARSE_NOBLANKS;
    }
    let recover = props.recover || options & f::XML_PARSE_RECOVER != 0;
    if recover {
        options |= f::XML_PARSE_RECOVER;
    }
    let (parsed, raised) = if from_file {
        if source.contains(&0) {
            return Err(arg_value_error(req, 1, "not contain any null bytes"));
        }
        let path = file_path(req, &source);
        Parser::xml(Source::File(&path), options, None, recover)
    } else {
        let dir = base_directory(req);
        Parser::xml(Source::Memory(&source), options, Some(&dir), recover)
    };
    Errors::route(req, raised);
    match parsed {
        Parsed::Document(doc) => Ok(finish_loading(req, this, doc)),
        Parsed::Nothing | Parsed::Malformed => Ok(Value::bool(false)),
    }
}

/// `DOMDocument::load(string $filename, int $options = 0): bool`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// file_put_contents($f, '<r><a/></r>');
/// $d = new DOMDocument;
/// var_dump($d->load($f), $d->documentElement->firstChild->nodeName, $d->documentURI === $f);
/// unlink($f);
/// try { $d->load(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(1) "a"
/// bool(true)
/// DOMDocument::load(): Argument #1 ($filename) must not be empty
/// ```
fn load(req: &mut Request) -> Result<Value, Thrown> {
    parse_document(req, true)
}

/// `DOMDocument::loadXML(string $source, int $options = 0): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->loadXML('<r a="1"><![CDATA[x]]></r>', LIBXML_NOCDATA));
/// echo $d->saveXML();
/// var_dump($d->loadXML('<a><b></a>'));
/// libxml_use_internal_errors(true);
/// var_dump($d->loadXML('<a>'), libxml_get_last_error()->message);
/// ```
/// ```expect
/// bool(true)
/// <?xml version="1.0"?>
/// <r a="1">x</r>
///
/// Warning: DOMDocument::loadXML(): Opening and ending tag mismatch: b line 1 and a in Entity, line: 1 in %s on line 5
///
/// Warning: DOMDocument::loadXML(): Premature end of data in tag a line 1 in Entity, line: 1 in %s on line 5
/// bool(false)
/// bool(false)
/// string(38) "Premature end of data in tag a line 1
/// "
/// ```
fn load_xml(req: &mut Request) -> Result<Value, Thrown> {
    parse_document(req, false)
}

/// `dom_load_html`.
fn load_html_as(req: &mut Request, from_file: bool) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let source = bytes_arg(req, 1)?;
    let options = long_arg(req, 2, 0)?;
    let this = this(req)?;
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let Ok(options) = i32::try_from(options) else {
        docref(req, Level::Warning, "Invalid options");
        return Ok(Value::bool(false));
    };
    let (doc, raised) = if from_file {
        if source.contains(&0) {
            return Err(arg_value_error(req, 1, "not contain any null bytes"));
        }
        Parser::html(Source::File(&source), options)
    } else {
        Parser::html(Source::Memory(&source), options)
    };
    Errors::route(req, raised);
    Ok(finish_loading(req, this, doc))
}

/// `DOMDocument::loadHTML(string $source, int $options = 0): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->loadHTML('<p>Hi<b>there</b></p>'));
/// echo $d->saveHTML();
/// echo $d->saveHTML($d->getElementsByTagName('b')->item(0)), "\n";
/// ```
/// ```expect
/// bool(true)
/// <!DOCTYPE html PUBLIC "-//W3C//DTD HTML 4.0 Transitional//EN" "http://www.w3.org/TR/REC-html40/loose.dtd">
/// <html><body><p>Hi<b>there</b></p></body></html>
/// <b>there</b>
/// ```
fn load_html(req: &mut Request) -> Result<Value, Thrown> {
    load_html_as(req, false)
}

/// `DOMDocument::loadHTMLFile(string $filename, int $options = 0): bool`.
fn load_html_file(req: &mut Request) -> Result<Value, Thrown> {
    load_html_as(req, true)
}

// ---- saving

/// `DOMDocument::saveXML(?DOMNode $node = null, int $options = 0): string|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r><a/><b>t</b></r>');
/// $first = $d->documentElement->firstChild;
/// echo $d->saveXML(), $d->saveXML(null, LIBXML_NOXMLDECL), $d->saveXML($first, LIBXML_NOEMPTYTAG), "\n";
/// try { $d->saveXML((new DOMDocument)->createElement('x')); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <?xml version="1.0"?>
/// <r><a/><b>t</b></r>
/// <r><a/><b>t</b></r>
/// <a></a>
/// Wrong Document Error
/// ```
fn save_xml(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 2)?;
    let node = object_arg(req, 1)?;
    let options = long_arg(req, 2, 0)?;
    let (this, doc) = this_node(req)?;
    let format = props_of(req, this).format;
    let no_empty = options & i64::from(php_ext_libxml::NOEMPTYTAG) != 0;
    let encoding = doc.doc_encoding();
    let (saved, raised) = if let Some(node) = node {
        let node = fetch(req, node)?;
        if node.doc() != Some(doc) {
            let strict = strict(req, this);
            dom_error(req, Code::WrongDocument, strict)?;
            return Ok(Value::bool(false));
        }
        Save::node(doc, node, format, no_empty, encoding.as_deref())
    } else {
        let mut converted = f::XML_SAVE_AS_XML;
        if options & i64::from(f::XML_SAVE_NO_DECL) != 0 {
            converted |= f::XML_SAVE_NO_DECL;
        }
        if format {
            converted |= f::XML_SAVE_FORMAT;
        }
        Save::doc(doc, converted, no_empty, encoding.as_deref())
    };
    Errors::route(req, raised);
    if let Some(bytes) = saved {
        string(req, &bytes)
    } else {
        docref(req, Level::Warning, "Could not save document");
        Ok(Value::bool(false))
    }
}

/// `DOMDocument::save(string $filename, int $options = 0): int|false`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// var_dump($d->save($f), file_get_contents($f));
/// unlink($f);
/// ```
/// ```expect
/// int(27)
/// string(27) "<?xml version="1.0"?>
/// <r/>
/// "
/// ```
fn save(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let file = path_arg(req, 1)?;
    let options = long_arg(req, 2, 0)?;
    if file.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let (this, doc) = this_node(req)?;
    let format = props_of(req, this).format;
    let no_empty = options & i64::from(php_ext_libxml::NOEMPTYTAG) != 0;
    let (bytes, raised) = Save::file(&file, doc, doc.doc_encoding().as_deref(), format, no_empty);
    Errors::route(req, raised);
    Ok(if bytes == -1 {
        Value::bool(false)
    } else {
        Value::int(bytes)
    })
}

/// `DOMDocument::saveHTML(?DOMNode $node = null): string|false`.
fn save_html(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let node = object_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    let format = props_of(req, this).format;
    let (saved, raised) = if let Some(node) = node {
        let node = fetch(req, node)?;
        if node.doc() != Some(doc) {
            let strict = strict(req, this);
            dom_error(req, Code::WrongDocument, strict)?;
            return Ok(Value::bool(false));
        }
        let nodes: Vec<Node> = if node.kind() == Kind::Fragment {
            let mut out = Vec::new();
            let mut at = node.children();
            while let Some(n) = at {
                out.push(n);
                at = n.next();
            }
            out
        } else {
            vec![node]
        };
        let (saved, raised) = Save::html_nodes(doc, &nodes, format);
        if saved.is_none() {
            Errors::route(req, raised);
            docref(req, Level::Warning, "Error dumping HTML node");
            return Ok(Value::bool(false));
        }
        (saved, raised)
    } else {
        Save::html_doc(doc, format)
    };
    Errors::route(req, raised);
    match saved {
        Some(bytes) => string(req, &bytes),
        None => Ok(Value::bool(false)),
    }
}

/// `DOMDocument::saveHTMLFile(string $filename): int|false`.
fn save_html_file(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let file = path_arg(req, 1)?;
    if file.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let (this, doc) = this_node(req)?;
    let format = props_of(req, this).format;
    let encoding = Save::meta_encoding(doc);
    let (bytes, raised) = Save::html_file(&file, doc, encoding.as_deref(), format);
    Errors::route(req, raised);
    Ok(if bytes == -1 {
        Value::bool(false)
    } else {
        Value::int(bytes)
    })
}

// ---- factories

/// A new node's object, or php's "Invalid State Error" where libxml made
/// none.
fn created(req: &mut Request, this: PhpObject, node: Option<Node>) -> Result<Value, Thrown> {
    match node {
        Some(n) => object_for(req, n, Some(this)),
        None => Err(crate::dom_throw(req, Code::InvalidState)),
    }
}

/// `INVALID_CHARACTER_ERR` for a name libxml refuses, `false` answered.
fn invalid_name(req: &mut Request, this: PhpObject) -> Result<Value, Thrown> {
    let strict = strict(req, this);
    dom_error(req, Code::InvalidCharacter, strict)?;
    Ok(Value::bool(false))
}

/// `DOMDocument::createElement(string $localName, string $value = "")`.
///
/// ```php
/// $d = new DOMDocument;
/// $e = $d->createElement('e', 'a &amp; b');
/// echo get_class($e), ' ', $d->saveXML($e), "\n";
/// try { $d->createElement('1x'); } catch (DOMException $e) { echo $e->getMessage(), ' ', $e->getCode(), "\n"; }
/// ```
/// ```expect
/// DOMElement <e>a &amp; b</e>
/// Invalid Character Error 5
/// ```
fn create_element(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let name = bytes_arg(req, 1)?;
    let value = crate::arg(req, 2)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let (this, doc) = this_node(req)?;
    if !Name::valid(&name) {
        return invalid_name(req, this);
    }
    let node = Node::new_element(Some(doc), None, &name, value.as_deref());
    created(req, this, node)
}

/// `DOMDocument::createDocumentFragment(): DOMDocumentFragment`.
fn create_document_fragment(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (this, doc) = this_node(req)?;
    created(req, this, Node::new_fragment(Some(doc)))
}

/// `DOMDocument::createTextNode(string $data): DOMText`.
///
/// ```php
/// $d = new DOMDocument;
/// foreach ([$d->createTextNode('a<b'), $d->createComment('c'), $d->createCDATASection('d]'), $d->createProcessingInstruction('p', 'q'), $d->createEntityReference('amp'), $d->createDocumentFragment()] as $n) {
///     echo get_class($n), ' ', $n->nodeName, "\n";
/// }
/// echo $d->saveXML($d->createTextNode('a<b')), "\n";
/// ```
/// ```expect
/// DOMText #text
/// DOMComment #comment
/// DOMCdataSection #cdata-section
/// DOMProcessingInstruction p
/// DOMEntityReference amp
/// DOMDocumentFragment #document-fragment
/// a&lt;b
/// ```
fn create_text_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    created(req, this, Node::new_text(Some(doc), &data))
}

/// `DOMDocument::createComment(string $data): DOMComment`.
fn create_comment(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    created(req, this, Node::new_comment(Some(doc), &data))
}

/// `DOMDocument::createCDATASection(string $data)`.
fn create_cdata_section(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    created(req, this, Node::new_cdata(Some(doc), &data))
}

/// `DOMDocument::createProcessingInstruction(string $target, string $data = "")`.
fn create_processing_instruction(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let target = bytes_arg(req, 1)?;
    let data = crate::arg(req, 2)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let (this, doc) = this_node(req)?;
    if !Name::valid(&target) {
        return invalid_name(req, this);
    }
    created(req, this, Node::new_pi(Some(doc), &target, data.as_deref()))
}

/// `DOMDocument::createAttribute(string $localName)`.
///
/// ```php
/// $d = new DOMDocument;
/// $a = $d->createAttribute('a');
/// var_dump(get_class($a), $a->name, $a->value);
/// try { $d->createAttribute("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(7) "DOMAttr"
/// string(1) "a"
/// string(0) ""
/// DOMDocument::createAttribute(): Argument #1 ($localName) must not contain any null bytes
/// ```
fn create_attribute(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = path_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    if !Name::valid(&name) {
        return invalid_name(req, this);
    }
    created(req, this, Node::new_attr(Some(doc), &name, None))
}

/// `DOMDocument::createEntityReference(string $name)`.
fn create_entity_reference(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    if !Name::valid(&name) {
        return invalid_name(req, this);
    }
    created(req, this, Node::new_reference(Some(doc), &name))
}

/// `DOMDocument::createElementNS(?string $namespace, string $qualifiedName, string $value = "")`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->appendChild($d->createElementNS('urn:a', 'p:r', 'v'));
/// $d->documentElement->appendChild($d->createElementNS('urn:a', 'p:c'));
/// echo $d->saveXML();
/// try { $d->createElementNS(null, 'p:x'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <?xml version="1.0"?>
/// <p:r xmlns:p="urn:a">v<p:c/></p:r>
/// Namespace Error
/// ```
fn create_element_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 3)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let value = crate::arg(req, 3)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let (this, doc) = this_node(req)?;
    let uri_len = uri.as_ref().map_or(0, Vec::len);
    let mut made = None;
    let answer = check_qname(&name, uri_len).and_then(|(local, prefix)| {
        if !Name::valid(&local) {
            return Err(Code::InvalidCharacter);
        }
        let Some(node) = Node::new_element(Some(doc), None, &local, value.as_deref()) else {
            return Err(Code::InvalidState);
        };
        made = Some(node);
        if let Some(uri) = &uri {
            let ns = match node.search_ns_by_href(node.doc(), uri) {
                Some(ns) => ns,
                None => get_ns(node, uri, prefix.as_deref())?,
            };
            node.set_ns(Some(ns));
        }
        Ok(node)
    });
    match answer {
        Ok(node) => object_for(req, node, Some(this)),
        Err(code) => {
            if let Some(n) = made {
                n.free();
            }
            let strict = strict(req, this);
            dom_error(req, code, strict)?;
            Ok(Value::bool(false))
        }
    }
}

/// `DOMDocument::createAttributeNS(?string $namespace, string $qualifiedName)`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->createAttributeNS('urn:a', 'p:a'));
/// $d->loadXML('<r/>');
/// $a = $d->createAttributeNS('urn:a', 'p:a');
/// $d->documentElement->setAttributeNodeNS($a);
/// $root = $d->documentElement;
/// echo $d->saveXML($root), "\n";
/// ```
/// ```expect
///
/// Warning: DOMDocument::createAttributeNS(): Document Missing Root Element in %s on line 3
/// bool(false)
/// <r xmlns:p="urn:a" p:a=""/>
/// ```
fn create_attribute_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let uri = nullable_bytes_arg(req, 1)?;
    let name = bytes_arg(req, 2)?;
    let (this, doc) = this_node(req)?;
    let Some(root) = doc.root() else {
        docref(req, Level::Warning, "Document Missing Root Element");
        return Ok(Value::bool(false));
    };
    let result = validate_and_extract(uri.as_deref(), &name)
        .map_err(|_| Code::Namespace)
        .and_then(|(local, prefix)| {
            let Some(node) = Node::new_attr(Some(doc), &local, None) else {
                return Err(Code::InvalidState);
            };
            if let Some(uri) = uri.as_deref().filter(|u| !u.is_empty()) {
                let found = root.search_ns_by_href(Some(doc), uri);
                let ns = if name == b"xmlns" || prefix.as_deref() == Some(b"xml") {
                    if let Some(ns) = found {
                        Some(ns)
                    } else {
                        let ns = Ns::new(None, Some(uri), prefix.as_deref());
                        if let Some(ns) = ns {
                            ns.move_to_old(Some(doc));
                        }
                        ns
                    }
                } else {
                    if let Some(ns) = found.filter(|ns| ns.prefix().is_some()) {
                        Some(ns)
                    } else {
                        let p = prefix.clone().unwrap_or_else(|| b"default".to_vec());
                        if let Some(ns) = get_ns_unchecked(root, uri, Some(&p)) {
                            Some(ns)
                        } else {
                            node.free();
                            return Err(Code::Namespace);
                        }
                    }
                };
                node.set_ns(ns);
            }
            Ok(node)
        });
    match result {
        Ok(node) => object_for(req, node, Some(this)),
        Err(code) => {
            let strict = strict(req, this);
            dom_error(req, code, strict)?;
            Ok(Value::bool(false))
        }
    }
}

/// `DOMDocument::getElementsByTagName(string $qualifiedName): DOMNodeList`,
/// shared with `DOMElement`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p"><a/><p:a/><b><a/></b></r>');
/// var_dump($d->getElementsByTagName('a')->length, $d->getElementsByTagName('*')->length, $d->getElementsByTagNameNS('urn:p', '*')->length, $d->getElementsByTagNameNS(null, 'a')->length, $d->documentElement->getElementsByTagName('a')->item(2)->parentNode->nodeName);
/// ```
/// ```expect
/// int(3)
/// int(5)
/// int(1)
/// int(2)
/// string(1) "b"
/// ```
fn get_elements_by_tag_name(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = path_arg(req, 1)?;
    let this = crate::this_intern(req)?;
    crate::create(
        req,
        this,
        "DOMNodeList",
        MapKind::ByTagName {
            local: name,
            ns: None,
        },
    )
}

/// `getElementsByTagNameNS(?string $namespace, string $localName): DOMNodeList`.
fn get_elements_by_tag_name_ns(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let ns = nullable_bytes_arg(req, 1)?.unwrap_or_default();
    let name = path_arg(req, 2)?;
    let this = crate::this_intern(req)?;
    crate::create(
        req,
        this,
        "DOMNodeList",
        MapKind::ByTagName {
            local: name,
            ns: Some(ns),
        },
    )
}

/// `DOMDocument::importNode(DOMNode $node, bool $deep = false)`.
///
/// ```php
/// $a = new DOMDocument; $a->loadXML('<r><c x="1">t</c></r>');
/// $b = new DOMDocument; $b->loadXML('<s/>');
/// $first = $a->documentElement->firstChild;
/// $n = $b->importNode($first, true);
/// $s = $b->documentElement;
/// $s->appendChild($n);
/// echo $b->saveXML($s), "\n";
/// var_dump($b->importNode($a));
/// ```
/// ```expect
/// <s><c x="1">t</c></s>
///
/// Warning: DOMDocument::importNode(): Cannot import: Node Type Not Supported in %s on line %d
/// bool(false)
/// ```
fn import_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let Some(node_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let deep = bool_arg(req, 2, false)?;
    let (this, doc) = this_node(req)?;
    let node = fetch(req, node_obj)?;
    if node.kind().is_document() {
        docref(
            req,
            Level::Warning,
            "Cannot import: Node Type Not Supported",
        );
        return Ok(Value::bool(false));
    }
    if node.doc() == Some(doc) {
        return object_for(req, node, Some(this));
    }
    let Some(copy) = crate::clone_node(node, Some(doc), deep) else {
        return Ok(Value::bool(false));
    };
    if copy.kind() == Kind::Attribute
        && copy.ns().is_none()
        && let Some(ns) = node.ns()
        && let Some(href) = ns.href()
    {
        let root = doc.root();
        let mut found = root.and_then(|r| r.search_ns_by_href(Some(doc), &href));
        if found.is_none_or(|n| n.prefix().is_none()) {
            found = root.and_then(|r| get_ns(r, &href, ns.prefix().as_deref()).ok());
        }
        copy.set_ns(found);
    }
    object_for(req, copy, Some(this))
}

/// `DOMDocument::getElementById(string $elementId): ?DOMElement`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r [<!ATTLIST e i ID #IMPLIED>]><r><e i="x"/><e id="y"/></r>');
/// var_dump($d->getElementById('x')->nodeName, $d->getElementById('y'));
/// $d->documentElement->lastChild->setIdAttribute('id', true);
/// var_dump($d->getElementById('y')->nodeName);
/// ```
/// ```expect
/// string(1) "e"
/// NULL
/// string(1) "e"
/// ```
fn get_element_by_id(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let id = bytes_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    let modified = dom(req, this).is_none_or(|d| !d.obj.has_document() || d.obj.modification() > 2);
    if !modified {
        let found = doc.get_id(&id).and_then(Node::parent);
        return nullable_object(req, found, Some(this));
    }
    let mut at = doc.children();
    while let Some(n) = at {
        if n.kind() == Kind::Element {
            let mut attr = n.properties();
            while let Some(a) = attr {
                if a.atype() == php_ext_libxml::ffi::XML_ATTRIBUTE_ID && a.attr_value() == id {
                    return object_for(req, n, Some(this));
                }
                attr = a.next();
            }
        }
        at = crate::next_in_tree_order(n, doc);
    }
    Ok(Value::null())
}

/// `DOMDocument::adoptNode(DOMNode $node): DOMNode|false`.
///
/// ```php
/// $a = new DOMDocument; $a->loadXML('<r><c/></r>');
/// $b = new DOMDocument; $b->loadXML('<s/>');
/// $first = $a->documentElement->firstChild;
/// $c = $b->adoptNode($first);
/// var_dump($c->ownerDocument === $b, $a->documentElement->hasChildNodes());
/// $s = $b->documentElement;
/// $s->appendChild($c);
/// echo $b->saveXML($s), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// <s><c/></s>
/// ```
fn adopt_node(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let Some(node_obj) = object_arg(req, 1)? else {
        return Ok(Value::bool(false));
    };
    let node = fetch(req, node_obj)?;
    if matches!(
        node.kind(),
        Kind::Document
            | Kind::HtmlDocument
            | Kind::DocumentType
            | Kind::Dtd
            | Kind::Entity
            | Kind::Notation
    ) {
        let strict = strict(req, node_obj);
        dom_error(req, Code::NotSupported, strict)?;
        return Ok(Value::bool(false));
    }
    let (this, doc) = this_node(req)?;
    if !adopt(req, this, node, doc) {
        return Ok(Value::bool(false));
    }
    let value = node_obj.as_value();
    req.addref(&value);
    Ok(value)
}

/// `DOMDocument::normalizeDocument(): void`.
fn normalize_document(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, doc) = this_node(req)?;
    crate::normalize(doc);
    Ok(Value::null())
}

/// `DOMDocument::xinclude(int $options = 0): int|false`.
fn xinclude(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let flags = long_arg(req, 1, 0)?;
    let Ok(flags) = i32::try_from(flags) else {
        docref(req, Level::Warning, "Invalid flags");
        return Ok(Value::bool(false));
    };
    let (this, doc) = this_node(req)?;
    let (err, raised) = Parser::xinclude(doc, flags);
    Errors::route(req, raised);
    invalidate(req, this);
    Ok(if err == 0 {
        Value::bool(false)
    } else {
        Value::int(i64::from(err))
    })
}

/// `DOMDocument::validate(): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r [<!ELEMENT r EMPTY>]><r/>');
/// var_dump($d->validate());
/// ```
/// ```expect
/// bool(true)
/// ```
fn validate(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, doc) = this_node(req)?;
    let (valid, raised) = Validate::dtd(doc);
    Errors::route(req, generic(raised));
    Ok(Value::bool(valid))
}

/// `dom_document_schema_validate` and `dom_document_relaxNG_validate`.
fn schema_validate_as(req: &mut Request, schema: Schema, from_file: bool) -> Result<Value, Thrown> {
    let max = if matches!(schema, Schema::Xsd) { 2 } else { 1 };
    arity(req, 1, max)?;
    let source = bytes_arg(req, 1)?;
    let flags = long_arg(req, 2, 0)?;
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let (_, doc) = this_node(req)?;
    let label = if matches!(schema, Schema::Xsd) {
        "Schema"
    } else {
        "RelaxNG"
    };
    let path;
    let source = if from_file {
        if source.contains(&0) {
            return Err(arg_value_error(req, 1, "not contain any null bytes"));
        }
        path = file_path(req, &source);
        Source::File(&path)
    } else {
        Source::Memory(&source)
    };
    let (answer, raised) = Validate::schema(doc, schema, source, i32::try_from(flags).unwrap_or(0));
    Errors::route(req, generic(raised));
    match answer {
        Validated::InvalidSchema => {
            docref(req, Level::Warning, &format!("Invalid {label}"));
            Ok(Value::bool(false))
        }
        Validated::Unusable => Err(throw(
            req,
            "Error",
            &format!("Invalid {label} Validation Context"),
        )),
        Validated::Done(valid) => Ok(Value::bool(valid)),
    }
}

fn schema_validate(req: &mut Request) -> Result<Value, Thrown> {
    schema_validate_as(req, Schema::Xsd, true)
}

fn schema_validate_source(req: &mut Request) -> Result<Value, Thrown> {
    schema_validate_as(req, Schema::Xsd, false)
}

fn relaxng_validate(req: &mut Request) -> Result<Value, Thrown> {
    schema_validate_as(req, Schema::RelaxNg, true)
}

fn relaxng_validate_source(req: &mut Request) -> Result<Value, Thrown> {
    schema_validate_as(req, Schema::RelaxNg, false)
}

/// `DOMDocument::registerNodeClass(string $baseClass, ?string $extendedClass): true`.
///
/// ```php
/// class E extends DOMElement {}
/// $d = new DOMDocument;
/// var_dump($d->registerNodeClass('DOMElement', 'E'));
/// $d->loadXML('<r/>');
/// echo get_class($d->documentElement), "\n";
/// $d->registerNodeClass('DOMElement', null);
/// echo get_class($d->createElement('x')), "\n";
/// try { $d->registerNodeClass('DOMElement', 'stdClass'); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// E
/// DOMElement
/// Error: DOMDocument::registerNodeClass(): Argument #2 ($extendedClass) must be a class name derived from DOMElement or null, stdClass given
/// ```
fn register_node_class(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let base_name = bytes_arg(req, 1)?;
    let extended = nullable_bytes_arg(req, 2)?;
    let this = crate::this_intern(req)?;
    let node_class = crate::class(req, "DOMNode")?;
    let base = match php_ext::call::lookup_class(req, &base_name)? {
        Some(c) if req.class_decl(c).is_some() && crate::class_extends(req, c, node_class) => c,
        Some(c) => {
            let shown = crate::class_name(req, c);
            return Err(arg_type_error(
                req,
                1,
                &format!("be a class name derived from DOMNode, {shown} given"),
            ));
        }
        None => {
            let shown = String::from_utf8_lossy(&base_name).into_owned();
            return Err(arg_type_error(
                req,
                1,
                &format!("be a valid class name, {shown} given"),
            ));
        }
    };
    let extended = match extended {
        None => None,
        Some(name) => {
            if let Some(c) = php_ext::call::lookup_class(req, &name)? {
                Some(c)
            } else {
                let shown = String::from_utf8_lossy(&name).into_owned();
                return Err(arg_type_error(
                    req,
                    2,
                    &format!("be a valid class name, {shown} given"),
                ));
            }
        }
    };
    let abstract_class = |req: &Request, c| req.class_decl(c).is_some_and(|d| d.flags.is_abstract);
    if abstract_class(req, base) {
        return Err(arg_value_error(req, 1, "not be an abstract class"));
    }
    if let Some(ext) = extended {
        if !crate::class_extends(req, ext, base) {
            let base_shown = crate::class_name(req, base);
            let ext_shown = crate::class_name(req, ext);
            return Err(php_ext::raising::argument_error(
                req,
                "Error",
                2,
                &format!(
                    "must be a class name derived from {base_shown} or null, {ext_shown} given"
                ),
            ));
        }
        if abstract_class(req, ext) {
            return Err(arg_value_error(req, 2, "not be an abstract class"));
        }
    }
    if let Some(p) = dom_mut(req, this).and_then(|d| d.obj.props_mut()) {
        p.classmap.retain(|(b, _)| *b != base);
        if let Some(ext) = extended {
            p.classmap.push((base, ext));
        }
    }
    Ok(Value::bool(true))
}

/// Whether the node object is of a legacy document class.
#[allow(dead_code)]
fn is_document(req: &mut Request, object: PhpObject) -> bool {
    instance_of(req, object, "DOMDocument")
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("DOMDocument::__construct", construct),
    ("DOMDocument::createAttribute", create_attribute),
    ("DOMDocument::createAttributeNS", create_attribute_ns),
    ("DOMDocument::createCDATASection", create_cdata_section),
    ("DOMDocument::createComment", create_comment),
    (
        "DOMDocument::createDocumentFragment",
        create_document_fragment,
    ),
    ("DOMDocument::createElement", create_element),
    ("DOMDocument::createElementNS", create_element_ns),
    (
        "DOMDocument::createEntityReference",
        create_entity_reference,
    ),
    (
        "DOMDocument::createProcessingInstruction",
        create_processing_instruction,
    ),
    ("DOMDocument::createTextNode", create_text_node),
    ("DOMDocument::getElementById", get_element_by_id),
    (
        "DOMDocument::getElementsByTagName",
        GET_ELEMENTS_BY_TAG_NAME,
    ),
    (
        "DOMDocument::getElementsByTagNameNS",
        GET_ELEMENTS_BY_TAG_NAME_NS,
    ),
    ("DOMDocument::importNode", import_node),
    ("DOMDocument::load", load),
    ("DOMDocument::loadXML", load_xml),
    ("DOMDocument::normalizeDocument", normalize_document),
    ("DOMDocument::registerNodeClass", register_node_class),
    ("DOMDocument::save", save),
    ("DOMDocument::loadHTML", load_html),
    ("DOMDocument::loadHTMLFile", load_html_file),
    ("DOMDocument::saveHTML", save_html),
    ("DOMDocument::saveHTMLFile", save_html_file),
    ("DOMDocument::saveXML", save_xml),
    ("DOMDocument::schemaValidate", schema_validate),
    ("DOMDocument::schemaValidateSource", schema_validate_source),
    ("DOMDocument::relaxNGValidate", relaxng_validate),
    (
        "DOMDocument::relaxNGValidateSource",
        relaxng_validate_source,
    ),
    ("DOMDocument::validate", validate),
    ("DOMDocument::xinclude", xinclude),
    ("DOMDocument::adoptNode", adopt_node),
    ("DOMDocument::append", crate::parent::APPEND),
    ("DOMDocument::prepend", crate::parent::PREPEND),
    (
        "DOMDocument::replaceChildren",
        crate::parent::REPLACE_CHILDREN,
    ),
];

/// `getElementsByTagName`, which `DOMElement` lists too.
pub(crate) const GET_ELEMENTS_BY_TAG_NAME: Entry = get_elements_by_tag_name;
/// `getElementsByTagNameNS`, which `DOMElement` lists too.
pub(crate) const GET_ELEMENTS_BY_TAG_NAME_NS: Entry = get_elements_by_tag_name_ns;

//! `DOMDocument` (`document.c`): the document object, its parse and save
//! switches, the node factories, loading and saving through libxml2, and
//! validation.

use php_ext::abi::{Instance, OrFalse, Passed, Path, Raw, This, True, Union, Untyped, Variadic};
use php_ext::attr::php_class;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi as f;
use php_ext_libxml::ffi::{
    DocString, Kind, Library, Name, Node, Ns, Parsed, Parser, Save, Schema, Source, Validate,
    Validated,
};

use crate::element::DOMElement;
use crate::map::MapKind;
use crate::node::DOMNode;
use crate::object::Code;
use crate::props::Prop;
use crate::{
    Read, adopt, check_qname, dom_error, dom_mut, fetch, file_path, get_ns, get_ns_unchecked,
    instance_of, invalidate, nullable_object, object_for, or_false, passed, props_of, returned,
    strict, string, validate_and_extract,
};

/// `XML_XML_NAMESPACE`.
pub(crate) const XML_NS: &[u8] = b"http://www.w3.org/XML/1998/namespace";
/// `DOM_XMLNS_NS_URI`.
pub(crate) const XMLNS_NS: &[u8] = b"http://www.w3.org/2000/xmlns/";

/// `zend_argument_must_not_be_empty_error`.
fn must_not_be_empty(req: &mut Request, position: u32) -> Thrown {
    arg_value_error(req, position, "not be empty")
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
pub(crate) fn doctype(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(nullable_object(req, doc.int_subset(), Some(object))?))
}

fn implementation(req: &mut Request, _object: PhpObject) -> Read {
    let class = crate::class(req, "DOMImplementation")?;
    Ok(Some(php_ext::call::new_object(req, class)?.into_value()))
}

/// `dom_document_document_element_read`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->documentElement);
/// $d->loadXML('<r><c/></r>');
/// $m = Dom\XMLDocument::createFromString('<s/>');
/// var_dump($d->documentElement->tagName, get_class($m->documentElement), $m->documentElement->tagName);
/// ```
/// ```expect
/// NULL
/// string(1) "r"
/// string(11) "Dom\Element"
/// string(1) "s"
/// ```
pub(crate) fn document_element(req: &mut Request, object: PhpObject) -> Read {
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
pub(crate) fn encoding(req: &mut Request, object: PhpObject) -> Read {
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
        _ => Err(throw_error(req, "ValueError", "Invalid document encoding")),
    }
}

/// `dom_document_standalone_read`.
///
/// ```php
/// $d = new DOMDocument;
/// var_dump($d->standalone, $d->xmlStandalone);
/// $d->loadXML('<?xml version="1.0" standalone="yes"?><r/>');
/// $m = Dom\XMLDocument::createFromString('<?xml version="1.0" standalone="no"?><r/>');
/// var_dump($d->standalone, $d->xmlStandalone, $m->xmlStandalone);
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn standalone(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(Owner::bool(doc.doc_standalone() > 0)))
}

fn set_standalone(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    doc.set_doc_standalone(i32::from(value.as_bool().unwrap_or(false)));
    Ok(())
}

/// `dom_document_version_read`.
///
/// ```php
/// $d = new DOMDocument('1.1');
/// $m = Dom\XMLDocument::createEmpty('1.1');
/// var_dump($d->version, $d->xmlVersion, $m->xmlVersion, Dom\XMLDocument::createFromString('<r/>')->xmlVersion);
/// ```
/// ```expect
/// string(3) "1.1"
/// string(3) "1.1"
/// string(3) "1.1"
/// string(3) "1.0"
/// ```
pub(crate) fn version(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    Ok(Some(crate::nullable_string(
        req,
        doc.doc_version().as_deref(),
    )?))
}

fn set_version(req: &mut Request, object: PhpObject, value: &Value) -> Result<(), Thrown> {
    let doc = doc_node(req, object)?;
    if crate::modern::follows_spec(req, object)
        && !matches!(value.as_str_bytes(), Some(b"1.0" | b"1.1"))
    {
        return Err(throw_error(req, "ValueError", "Invalid XML version"));
    }
    doc.set_doc_string(
        DocString::Version,
        Some(value.as_str_bytes().unwrap_or_default()),
    );
    Ok(())
}

/// `dom_document_document_uri_read`: `about:blank` for a modern document
/// with no URL.
///
/// ```php
/// var_dump((new DOMDocument)->documentURI, Dom\XMLDocument::createEmpty()->URL, Dom\HTMLDocument::createEmpty()->documentURI);
/// ```
/// ```expect
/// NULL
/// string(11) "about:blank"
/// string(11) "about:blank"
/// ```
pub(crate) fn document_uri(req: &mut Request, object: PhpObject) -> Read {
    let doc = doc_node(req, object)?;
    match doc.doc_url() {
        Some(url) => Ok(Some(string(req, &url)?)),
        None if crate::modern::follows_spec(req, object) => Ok(Some(string(req, b"about:blank")?)),
        None => Ok(Some(Owner::null())),
    }
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
    Ok(Some(Owner::null()))
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
    Ok(Some(Owner::bool(match which {
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

/// `URL` of `Dom\Document`.
pub(crate) const MODERN_URL: Prop = rw(b"URL", document_uri, set_document_uri);

/// `documentURI` of `Dom\Document`.
pub(crate) const MODERN_DOCUMENT_URI: Prop = rw(b"documentURI", document_uri, set_document_uri);

/// `dom_xml_document_prop_handlers`' own rows, `Dom\XMLDocument`'s.
pub(crate) const MODERN_XML_PROPS: &[Prop] = &[
    ro(b"xmlEncoding", encoding),
    rw(b"xmlStandalone", standalone, set_standalone),
    rw(b"xmlVersion", version, set_version),
    rw(b"formatOutput", format_read, format_write),
];

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

/// `php_dom_finish_loading_document`: the object takes the new tree, the
/// old one let go of, the switches and the class map carried over.
fn finish_loading(req: &mut Request, this: PhpObject, doc: Option<Node>) -> bool {
    let Some(doc) = doc else {
        return false;
    };
    if let Some(mut obj) = crate::libxml_half(req, this) {
        let mut class_type = php_ext_libxml::ffi::ClassType::Legacy;
        let mut modification = 0;
        let mut props = None;
        if obj.is_bound() {
            class_type = obj.class_type();
            modification = obj.modification();
            obj.decrement_node_ptr(req);
            props = obj.take_props();
            obj.decrement_doc_ref(req);
        }
        obj.adopt_document(req, doc);
        obj.put_props(props);
        obj.set_class_type(class_type);
        obj.take_node(req, doc);
        if modification != 0 {
            obj.set_modification(modification);
            obj.invalidate();
        }
    }
    true
}

/// The directory a document parsed from memory takes as its base, php's
/// `VCWD_GETCWD` with its trailing slash.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// $m = Dom\XMLDocument::createFromString('<r/>');
/// var_dump($d->documentURI === getcwd() . '/', $m->documentURI === getcwd() . '/');
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn base_directory(req: &Request) -> Vec<u8> {
    let mut dir = req.cwd().to_string_lossy().into_owned().into_bytes();
    if !dir.ends_with(b"/") {
        dir.push(b'/');
    }
    dir
}

/// `dom_parse_document`.
fn parse_document(
    req: &mut Request,
    this: PhpObject,
    source: &[u8],
    options: i64,
    from_file: bool,
) -> Result<bool, Thrown> {
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let Ok(options) = i32::try_from(options) else {
        docref(req, Level::Warning, "Invalid options");
        return Ok(false);
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
    let (parsed, raised, thrown) = if from_file {
        if source.contains(&0) {
            return Err(arg_value_error(req, 1, "not contain any null bytes"));
        }
        let Some(path) = file_path(req, source) else {
            return Ok(false);
        };
        Parser::xml(req, Source::File(&path), options, None, recover, None)
    } else {
        let dir = base_directory(req);
        Parser::xml(
            req,
            Source::Memory(source),
            options,
            Some(&dir),
            recover,
            None,
        )
    };
    let settled = Errors::settle(req, raised, thrown);
    let loaded = match parsed {
        Parsed::Document(doc) => finish_loading(req, this, doc),
        Parsed::Nothing | Parsed::Malformed => false,
    };
    settled.map(|()| loaded)
}

/// `dom_load_html`.
fn load_html_as(
    req: &mut Request,
    this: PhpObject,
    source: &[u8],
    options: i64,
    from_file: bool,
) -> Result<bool, Thrown> {
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let Ok(options) = i32::try_from(options) else {
        docref(req, Level::Warning, "Invalid options");
        return Ok(false);
    };
    let (doc, raised, thrown) = if from_file {
        if source.contains(&0) {
            return Err(arg_value_error(req, 1, "not contain any null bytes"));
        }
        Parser::html(req, Source::File(source), options)
    } else {
        Parser::html(req, Source::Memory(source), options)
    };
    let settled = Errors::settle(req, raised, thrown);
    let loaded = finish_loading(req, this, doc);
    settled.map(|()| loaded)
}

// ---- saving

// ---- factories

/// A new node's object, or php's "Invalid State Error" where libxml made
/// none.
fn created(req: &mut Request, this: PhpObject, node: Option<Node>) -> Result<Owner<Value>, Thrown> {
    match node {
        Some(n) => object_for(req, n, Some(this)),
        None => Err(crate::dom_throw(req, Code::InvalidState)),
    }
}

/// `INVALID_CHARACTER_ERR` for a name libxml refuses, `false` answered.
fn invalid_name(req: &mut Request, this: PhpObject) -> Result<Owner<Value>, Thrown> {
    let strict = strict(req, this);
    dom_error(req, Code::InvalidCharacter, strict)?;
    Ok(Owner::bool(false))
}

/// The legacy branch of `DOMDocument::createAttributeNS` past the name's
/// check: the attribute, its namespace one the root declares or a fresh
/// declaration on it.
fn legacy_attribute_ns(
    doc: Node,
    root: Option<Node>,
    uri: Option<&[u8]>,
    name: &[u8],
    local: &[u8],
    prefix: Option<&[u8]>,
) -> Result<Node, Code> {
    let Some(node) = Node::new_attr(Some(doc), local, None) else {
        return Err(Code::InvalidState);
    };
    if let Some(uri) = uri.filter(|u| !u.is_empty())
        && let Some(root) = root
    {
        let found = root.search_ns_by_href(Some(doc), uri);
        let ns = if name == b"xmlns" || prefix == Some(b"xml") {
            if let Some(ns) = found {
                Some(ns)
            } else {
                let ns = Ns::new(None, Some(uri), prefix);
                if let Some(ns) = ns {
                    ns.move_to_old(Some(doc));
                }
                ns
            }
        } else {
            if let Some(ns) = found.filter(|ns| ns.prefix().is_some()) {
                Some(ns)
            } else {
                let p = prefix.unwrap_or(b"default");
                if let Some(ns) = get_ns_unchecked(root, uri, Some(p)) {
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
}

/// `dom_element_get_elements_by_tag_name_ns`: a list of `class` over the
/// elements of one namespace and local name, `null` for no namespace.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:p="urn:p"><a/><p:a/><p:b/></r>');
/// $m = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><a/><p:a/><p:b/></r>');
/// $l = $d->getElementsByTagNameNS('urn:p', '*');
/// $h = $m->getElementsByTagNameNS(null, 'a');
/// var_dump(get_class($l), $l->length, get_class($h), $h->length, $m->documentElement->getElementsByTagNameNS('*', 'a')->length);
/// try { $d->getElementsByTagNameNS("a\0", 'b'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(11) "DOMNodeList"
/// int(2)
/// string(18) "Dom\HTMLCollection"
/// int(1)
/// int(2)
/// DOMDocument::getElementsByTagNameNS(): Argument #1 ($namespace) must not contain any null bytes
/// ```
pub(crate) fn elements_by_tag_name_ns(
    req: &mut Request,
    this: PhpObject,
    namespace: Option<Path>,
    local_name: Path,
    class: &str,
) -> Result<Owner<Value>, Thrown> {
    let ns = namespace
        .map(|n| n.0.as_bytes().to_vec())
        .unwrap_or_default();
    let this = crate::intern(req, this)?;
    crate::create(
        req,
        this,
        class,
        MapKind::ByTagName {
            local: local_name.0.as_bytes().to_vec(),
            ns: Some(ns),
            qname: false,
        },
    )
}

/// `dom_xinclude_strip_fallback_references` (GH-14702, GH-17847): every
/// object standing for a node under an `xi:include` let go of before
/// libxml2 clones the include's nodes and frees the originals, so such an
/// object answers `Invalid State Error` rather than reading freed memory.
fn strip_xinclude_references(req: &mut Request, doc: Node) {
    const XINCLUDE_NS: &[u8] = b"http://www.w3.org/2001/XInclude";
    const XINCLUDE_OLD_NS: &[u8] = b"http://www.w3.org/2003/XInclude";
    let mut current = doc.children();
    while let Some(node) = current {
        if node.kind() == Kind::Element
            && node.name() == b"include"
            && node
                .ns()
                .and_then(Ns::href)
                .is_some_and(|h| h == XINCLUDE_NS || h == XINCLUDE_OLD_NS)
        {
            strip_references(req, node);
        }
        current = crate::next_in_tree_order(node, doc);
    }
}

/// `dom_xinclude_strip_references`: the node, its attributes and every
/// node below it unregistered from the objects standing for them.
fn strip_references(req: &mut Request, base: Node) {
    let strip_attributes = |req: &mut Request, element: Node| {
        let mut attr = element.properties();
        while let Some(a) = attr {
            a.free_resource(req);
            let mut child = a.children();
            while let Some(c) = child {
                c.free_resource(req);
                child = c.next();
            }
            attr = a.next();
        }
    };
    base.free_resource(req);
    strip_attributes(req, base);
    let mut current = base.children();
    while let Some(node) = current {
        node.free_resource(req);
        if node.kind() == Kind::Element {
            strip_attributes(req, node);
        }
        current = crate::next_in_tree_order(node, base);
    }
}

/// `dom_validate_ns_guard_begin`: a document of the modern classes keeps
/// its declarations as `xmlns` attributes, which libxml's validators do
/// not read, so they are lent back to `nsDef` while one runs and taken
/// back when the guard drops (`dom_validate_ns_guard_end`).
fn ns_guard(req: &Request, this: PhpObject, doc: Node) -> Option<crate::node::NsLinks> {
    crate::modern::follows_spec(req, this).then(|| crate::node::NsLinks::relink(doc.root()))
}

/// `dom_document_schema_validate` and `dom_document_relaxNG_validate`.
fn schema_validate_as(
    req: &mut Request,
    this: PhpObject,
    (source, flags): (&[u8], i64),
    schema: Schema,
    from_file: bool,
) -> Result<bool, Thrown> {
    if source.is_empty() {
        return Err(must_not_be_empty(req, 1));
    }
    let doc = fetch(req, this)?;
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
        let Some(resolved) = file_path(req, source) else {
            docref(req, Level::Warning, &format!("Invalid {label} file source"));
            return Ok(false);
        };
        path = resolved;
        Source::File(&path)
    } else {
        Source::Memory(source)
    };
    let guard = ns_guard(req, this, doc);
    let (answer, raised, thrown) =
        Validate::schema(req, doc, schema, source, i32::try_from(flags).unwrap_or(0));
    drop(guard);
    Errors::settle(req, raised, thrown)?;
    match answer {
        Validated::InvalidSchema => {
            docref(req, Level::Warning, &format!("Invalid {label}"));
            Ok(false)
        }
        Validated::Unusable => Err(throw_error(
            req,
            "Error",
            &format!("Invalid {label} Validation Context"),
        )),
        Validated::Done(valid) => Ok(valid),
    }
}

/// `dom_document_register_node_class`.
///
/// Both classes are `Z_PARAM_CLASS`, `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy extends DOMElement {} } });
/// $doc = new DOMDocument;
/// $doc->registerNodeClass('DOMElement', 'Lazy');
/// echo get_class($doc->createElement('a')), "\n";
/// ```
/// ```expect
/// load Lazy
/// Lazy
/// ```
fn register_node_class_as(
    req: &mut Request,
    this: PhpObject,
    base_name: &[u8],
    extended: Option<&[u8]>,
    modern: bool,
) -> Result<(), Thrown> {
    let this = crate::intern(req, this)?;
    let node_name = if modern { "Dom\\Node" } else { "DOMNode" };
    let node_class = crate::class(req, node_name)?;
    let base = match php_ext::call::autoload(req, base_name)? {
        Some(c) if req.class_decl(c).is_some() && crate::class_extends(req, c, node_class) => c,
        Some(c) => {
            let shown = crate::class_name(req, c);
            return Err(arg_type_error(
                req,
                1,
                &format!("be a class name derived from {node_name}, {shown} given"),
            ));
        }
        None => {
            let shown = String::from_utf8_lossy(base_name).into_owned();
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
            if let Some(c) = php_ext::call::autoload(req, name)? {
                Some(c)
            } else {
                let shown = String::from_utf8_lossy(name).into_owned();
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
    Ok(())
}

/// Whether the node object is of a legacy document class.
#[allow(dead_code)]
fn is_document(req: &mut Request, object: PhpObject) -> bool {
    instance_of(req, object, "DOMDocument")
}

/// `DOMDocument::saveXML`'s body, which the `Dom\` documents' `saveXml`
/// shares: the document's handlers dump it (`dom_document_save_xml`),
/// libxml's own for a legacy document and php's serialiser for a modern
/// one.
pub(crate) fn save_xml(
    req: &mut Request,
    this: PhpObject,
    node: Option<PhpObject>,
    options: i64,
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let format = props_of(req, this).format;
    let no_empty = options & i64::from(php_ext_libxml::NOEMPTYTAG) != 0;
    let encoding = doc.doc_encoding();
    let handlers = handlers_of(req, this);
    let saved = if let Some(node) = node {
        let node = fetch(req, node)?;
        if node.doc() != Some(doc) {
            let strict = strict(req, this);
            dom_error(req, Code::WrongDocument, strict)?;
            return Ok(Owner::bool(false));
        }
        handlers.dump_node_to_str(req, doc, node, format, no_empty, encoding.as_deref())?
    } else {
        let mut converted = f::XML_SAVE_AS_XML;
        if options & i64::from(f::XML_SAVE_NO_DECL) != 0 {
            converted |= f::XML_SAVE_NO_DECL;
        }
        if format {
            converted |= f::XML_SAVE_FORMAT;
        }
        handlers.dump_doc_to_str(req, doc, converted, no_empty, encoding.as_deref())?
    };
    if let Some(bytes) = saved {
        string(req, &bytes)
    } else {
        docref(req, Level::Warning, "Could not save document");
        Ok(Owner::bool(false))
    }
}

/// `intern->document->handlers` of `object`, libxml's own where the
/// object counts no document.
fn handlers_of(req: &Request, object: PhpObject) -> &'static dyn php_ext_libxml::DocumentHandlers {
    crate::dom(req, object).map_or(&php_ext_libxml::DefaultHandlers, |d| {
        d.obj.document_handlers()
    })
}

/// `DOMDocument::saveHTML`'s body.
fn save_html(
    req: &mut Request,
    this: PhpObject,
    node: Option<PhpObject>,
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let format = props_of(req, this).format;
    let (saved, raised) = if let Some(node) = node {
        let node = fetch(req, node)?;
        if node.doc() != Some(doc) {
            let strict = strict(req, this);
            dom_error(req, Code::WrongDocument, strict)?;
            return Ok(Owner::bool(false));
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
            return Ok(Owner::bool(false));
        }
        (saved, raised)
    } else {
        Save::html_doc(doc, format)
    };
    Errors::route(req, raised);
    match saved {
        Some(bytes) => string(req, &bytes),
        None => Ok(Owner::bool(false)),
    }
}

/// `DOMDocument::createElementNS`'s body.
fn create_element_ns(
    req: &mut Request,
    this: PhpObject,
    uri: Option<&[u8]>,
    name: &[u8],
    value: Option<&[u8]>,
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let uri_len = uri.map_or(0, <[u8]>::len);
    let mut made = None;
    let answer = check_qname(name, uri_len).and_then(|(local, prefix)| {
        if !Name::valid(&local) {
            return Err(Code::InvalidCharacter);
        }
        let Some(node) = Node::new_element(Some(doc), None, &local, value) else {
            return Err(Code::InvalidState);
        };
        made = Some(node);
        if let Some(uri) = uri {
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
            Ok(Owner::bool(false))
        }
    }
}

/// `DOMDocument::createAttributeNS`'s body, `Dom\Document`'s too.
fn create_attribute_ns(
    req: &mut Request,
    this: PhpObject,
    namespace: Option<PhpString>,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let uri = namespace.as_ref().map(PhpString::as_bytes);
    let doc = fetch(req, this)?;
    let modern = crate::modern::follows_spec(req, this);
    let root = doc.root();
    if root.is_none() && !modern {
        docref(req, Level::Warning, "Document Missing Root Element");
        return Ok(Owner::bool(false));
    }
    let parts = match validate_and_extract(uri, name) {
        Ok(parts) => parts,
        Err(code) => {
            let code = if modern { code } else { Code::Namespace };
            let strict = strict(req, this);
            dom_error(req, code, strict)?;
            return Ok(Owner::bool(false));
        }
    };
    if modern {
        let (local, prefix) = parts;
        let Some(node) = Node::new_attr(Some(doc), &local, None) else {
            return Err(crate::dom_throw(req, Code::InvalidState));
        };
        if let Some(namespace) = namespace.filter(|u| !u.is_empty()) {
            let ns =
                crate::element::mapped_ns(req, this, Some(doc), prefix.as_deref(), Some(namespace));
            let ns = match ns {
                Ok(ns) => ns,
                Err(thrown) => {
                    node.free();
                    return Err(thrown);
                }
            };
            node.set_ns(ns);
        }
        return object_for(req, node, Some(this));
    }
    let (local, prefix) = parts;
    let result = legacy_attribute_ns(doc, root, uri, name, &local, prefix.as_deref());
    match result {
        Ok(node) => object_for(req, node, Some(this)),
        Err(code) => {
            let strict = strict(req, this);
            dom_error(req, code, strict)?;
            Ok(Owner::bool(false))
        }
    }
}

/// `DOMDocument::importNode`'s body.
fn import_node(
    req: &mut Request,
    this: PhpObject,
    node_obj: PhpObject,
    deep: bool,
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let node = fetch(req, node_obj)?;
    if node.kind().is_document() {
        docref(
            req,
            Level::Warning,
            "Cannot import: Node Type Not Supported",
        );
        return Ok(Owner::bool(false));
    }
    if node.doc() == Some(doc) {
        return object_for(req, node, Some(this));
    }
    let Some(copy) = crate::clone_node(node, Some(doc), deep) else {
        return Ok(Owner::bool(false));
    };
    if copy.kind() == Kind::Attribute
        && copy.ns().is_none()
        && let Some(ns) = node.ns()
        && let Some(href) = ns.href()
    {
        let root = doc.root();
        let mut found = root.and_then(|r| r.search_ns_by_href(Some(doc), &href));
        if found.is_none_or(|n| n.prefix().is_none()) {
            found = if let Some(r) = root {
                get_ns(r, &href, ns.prefix().as_deref()).ok()
            } else {
                // No root to declare it on: `dom_get_ns(NULL, ...)` makes a
                // declaration nothing holds, and the document's old list
                // takes it.
                let detached = Ns::new(None, Some(&href), ns.prefix().as_deref());
                if let Some(d) = detached {
                    d.move_to_old(Some(doc));
                }
                detached
            };
        }
        copy.set_ns(found);
    }
    object_for(req, copy, Some(this))
}

/// `dom_modern_document_import_node`: `Dom\Document::importNode` and
/// `importLegacyNode`, the copy made into this document with its
/// namespaces the mapper's; a document refused, a failed copy an
/// `Invalid State Error`.
fn modern_import(
    req: &mut Request,
    this: PhpObject,
    node_obj: Option<PhpObject>,
    deep: bool,
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let Some(node_obj) = node_obj else {
        // `?Dom\Node $node` lets `null` through to `O`, which refuses it.
        return Err(arg_type_error(req, 1, "be of type Dom\\Node, null given"));
    };
    let node = fetch(req, node_obj)?;
    if node.kind().is_document() {
        return Err(crate::dom_throw(req, Code::NotSupported));
    }
    if node.doc() == Some(doc) {
        return object_for(req, node, Some(this));
    }
    let copy = crate::private::mapped(req, this, |mapper, req| {
        crate::node::clone_node_modern(req, mapper, node, Some(doc), deep)
    })?;
    let Some(copy) = copy else {
        return Err(crate::dom_throw(req, Code::InvalidState));
    };
    object_for(req, copy, Some(this))
}

/// `DOMDocument::getElementById`'s body, `Dom\Document`'s too.
fn get_element_by_id(
    req: &mut Request,
    this: PhpObject,
    id: &[u8],
) -> Result<Owner<Value>, Thrown> {
    let doc = fetch(req, this)?;
    let modified = crate::modified_since_parsing(req, this);
    if !modified {
        let found = doc.get_id(id).and_then(Node::parent);
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
    Ok(Owner::null())
}

/// `dom_document_adopt_node`.
fn adopt_node(
    req: &mut Request,
    this: PhpObject,
    node_obj: PhpObject,
    modern: bool,
) -> Result<Owner<Value>, Thrown> {
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
        return Ok(Owner::bool(false));
    }
    let doc = fetch(req, this)?;
    if !adopt(req, this, node, doc)? {
        if modern {
            return Err(crate::dom_throw(req, Code::InvalidState));
        }
        return Ok(Owner::bool(false));
    }
    let value = node_obj.as_value();
    Ok(Owner::take(req, value))
}

/// The first steps of `DOMDocument::xinclude` and `Dom\XMLDocument`'s,
/// the flags past an `int` refused by the caller: the includes' objects
/// let go of, the substitution made, the lists invalidated; libxml's
/// count of substitutions, `-1` for a failure.
fn run_xinclude(req: &mut Request, this: PhpObject, flags: i32) -> Result<i32, Thrown> {
    let doc = fetch(req, this)?;
    strip_xinclude_references(req, doc);
    let (err, raised, thrown) = Parser::xinclude(req, doc, flags);
    let settled = Errors::settle(req, raised, thrown);
    invalidate(req, this);
    settled?;
    Ok(err)
}

/// `class DOMDocument extends DOMNode implements DOMParentNode`: the
/// legacy document, its twenty-two properties `@virtual`, the loaders,
/// the savers, the factories and the validators.
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
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// file_put_contents($f, '<p>a</p>');
/// $d = new DOMDocument;
/// var_dump($d->loadHTMLFile($f, LIBXML_HTML_NOIMPLIED | LIBXML_HTML_NODEFDTD), $d->saveHTML());
/// var_dump($d->saveHTMLFile($f), file_get_contents($f));
/// $x = new DOMDocument;
/// $x->loadXML('<r>a</r>');
/// $x->documentElement->appendChild($x->createTextNode('b'));
/// $x->normalizeDocument();
/// var_dump($x->documentElement->childNodes->length);
/// file_put_contents($f, '<schema xmlns="http://www.w3.org/2001/XMLSchema"><element name="r" type="string"/></schema>');
/// var_dump($x->schemaValidate($f));
/// $rng = '<element name="r" xmlns="http://relaxng.org/ns/structure/1.0"><text/></element>';
/// var_dump($x->relaxNGValidateSource($rng));
/// file_put_contents($f, $rng);
/// var_dump($x->relaxNGValidate($f));
/// unlink($f);
/// ```
/// ```expect
/// bool(true)
/// string(9) "<p>a</p>
/// "
/// int(9)
/// string(9) "<p>a</p>
/// "
/// int(1)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_class(extends(DOMNode), implements(crate::parent::DOMParentNode))]
pub(crate) struct DOMDocument;

#[php_class]
impl DOMDocument {
    #[php(property, virtual)]
    const doctype: Option<Instance<crate::classes::DOMDocumentType>>;
    #[php(property, virtual)]
    const implementation: Instance<crate::classes::DOMImplementation>;
    #[php(property, virtual)]
    const documentElement: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const actualEncoding: Option<PhpString>;
    #[php(property, virtual)]
    const encoding: Option<PhpString>;
    #[php(property, virtual)]
    const xmlEncoding: Option<PhpString>;
    #[php(property, virtual)]
    const standalone: bool;
    #[php(property, virtual)]
    const xmlStandalone: bool;
    #[php(property, virtual)]
    const version: Option<PhpString>;
    #[php(property, virtual)]
    const xmlVersion: Option<PhpString>;
    #[php(property, virtual)]
    const strictErrorChecking: bool;
    #[php(property, virtual)]
    const documentURI: Option<PhpString>;
    #[php(property, virtual)]
    const config: Value;
    #[php(property, virtual)]
    const formatOutput: bool;
    #[php(property, virtual)]
    const validateOnParse: bool;
    #[php(property, virtual)]
    const resolveExternals: bool;
    #[php(property, virtual)]
    const preserveWhiteSpace: bool;
    #[php(property, virtual)]
    const recover: bool;
    #[php(property, virtual)]
    const substituteEntities: bool;
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const childElementCount: i64;

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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "1.0")] version: PhpString,
        #[php(default = "")] encoding: PhpString,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let Some(doc) = Node::new_doc(Some(version.as_bytes())) else {
            return Err(crate::dom_throw(req, Code::InvalidState));
        };
        if !encoding.is_empty() {
            doc.set_doc_string(DocString::Encoding, Some(encoding.as_bytes()));
        }
        if let Some(mut obj) = crate::libxml_half(req, this) {
            obj.release(req);
            obj.adopt_document(req, doc);
            obj.take_node(req, doc);
        }
        Ok(())
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
    ///
    /// ```php
    /// $a = Dom\HTMLDocument::createEmpty()->createAttribute('Data-A');
    /// $b = Dom\XMLDocument::createEmpty()->createAttribute('Data-A');
    /// var_dump(get_class($a), $a->name, $b->name, $b->namespaceURI);
    /// ```
    /// ```expect
    /// string(8) "Dom\Attr"
    /// string(6) "data-a"
    /// string(6) "Data-A"
    /// NULL
    /// ```
    fn createAttribute(
        req: &mut Request,
        this: This<Self>,
        localName: Path,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let name = localName.0.as_bytes();
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(name) {
            return invalid_name(req, this).map(Untyped);
        }
        let name = if doc.kind() == Kind::HtmlDocument && crate::modern::follows_spec(req, this) {
            name.to_ascii_lowercase()
        } else {
            name.to_vec()
        };
        created(req, this, Node::new_attr(Some(doc), &name, None)).map(Untyped)
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
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createEmpty();
    /// $a = $d->createAttributeNS('urn:a', 'p:a');
    /// $b = $d->createAttributeNS(null, 'b');
    /// var_dump(get_class($a), $a->prefix, $a->localName, $a->namespaceURI, $b->namespaceURI);
    /// try { $d->createAttributeNS(null, 'p:x'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// try { $d->createAttributeNS('urn:a', '1'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// try { $d->createAttributeNS('urn:a', 'xmlns'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(8) "Dom\Attr"
    /// string(1) "p"
    /// string(1) "a"
    /// string(5) "urn:a"
    /// NULL
    /// Namespace Error
    /// Invalid Character Error
    /// Namespace Error
    /// ```
    fn createAttributeNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        create_attribute_ns(req, this.object(), namespace, qualifiedName.as_bytes()).map(Untyped)
    }

    /// `DOMDocument::createCDATASection(string $data)`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createEmpty();
    /// $c = $d->createCDATASection('a]]b');
    /// var_dump(get_class($c), $c->data);
    /// try { $d->createCDATASection('a]]>b'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { Dom\HTMLDocument::createEmpty()->createCDATASection('x'); } catch (DOMException $e) { echo $e->getMessage(), ' ', $e->code, "\n"; }
    /// ```
    /// ```expect
    /// string(16) "Dom\CDATASection"
    /// string(4) "a]]b"
    /// Invalid character sequence "]]>" in CDATA section
    /// This operation is not supported for HTML documents 9
    /// ```
    fn createCDATASection(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let data = data.as_bytes();
        let this = this.object();
        let doc = fetch(req, this)?;
        if crate::modern::follows_spec(req, this) {
            if doc.kind() == Kind::HtmlDocument {
                return Err(crate::dom_throw_message(
                    req,
                    Code::NotSupported,
                    "This operation is not supported for HTML documents",
                ));
            }
            if data.windows(3).any(|w| w == b"]]>") {
                return Err(crate::dom_throw_message(
                    req,
                    Code::InvalidCharacter,
                    "Invalid character sequence \"]]>\" in CDATA section",
                ));
            }
        }
        created(req, this, Node::new_cdata(Some(doc), data)).map(Untyped)
    }

    /// `DOMDocument::createComment(string $data): DOMComment`.
    #[php(tentative)]
    fn createComment(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::classes::DOMComment>>>, Thrown> {
        let this = this.object();
        let doc = fetch(req, this)?;
        let made = created(req, this, Node::new_comment(Some(doc), data.as_bytes()))?;
        returned(made)
    }

    /// `DOMDocument::createDocumentFragment(): DOMDocumentFragment`.
    #[php(tentative)]
    fn createDocumentFragment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Raw<Instance<crate::classes::DOMDocumentFragment>>>, Thrown> {
        let this = this.object();
        let doc = fetch(req, this)?;
        let made = created(req, this, Node::new_fragment(Some(doc)))?;
        returned(made)
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
    fn createElement(
        req: &mut Request,
        this: This<Self>,
        localName: PhpString,
        #[php(default = "")] value: Passed<PhpString>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let name = localName.as_bytes();
        let value = passed(value);
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(name) {
            return invalid_name(req, this).map(Untyped);
        }
        let node = Node::new_element(Some(doc), None, name, value.as_deref());
        created(req, this, node).map(Untyped)
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
    fn createElementNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
        #[php(default = "")] value: Passed<PhpString>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let uri = namespace.as_ref().map(PhpString::as_bytes);
        let value = passed(value);
        create_element_ns(
            req,
            this.object(),
            uri,
            qualifiedName.as_bytes(),
            value.as_deref(),
        )
        .map(Untyped)
    }

    /// `DOMDocument::createEntityReference(string $name)`.
    fn createEntityReference(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let name = name.as_bytes();
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(name) {
            return invalid_name(req, this).map(Untyped);
        }
        created(req, this, Node::new_reference(Some(doc), name)).map(Untyped)
    }

    /// `DOMDocument::createProcessingInstruction(string $target, string $data = "")`.
    fn createProcessingInstruction(
        req: &mut Request,
        this: This<Self>,
        target: PhpString,
        #[php(default = "")] data: Passed<PhpString>,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let target = target.as_bytes();
        let data = passed(data);
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(target) {
            return invalid_name(req, this).map(Untyped);
        }
        created(req, this, Node::new_pi(Some(doc), target, data.as_deref())).map(Untyped)
    }

    /// `DOMDocument::createTextNode(string $data): DOMText`.
    #[php(tentative)]
    fn createTextNode(
        req: &mut Request,
        this: This<Self>,
        data: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::classes::DOMText>>>, Thrown> {
        let this = this.object();
        let doc = fetch(req, this)?;
        let made = created(req, this, Node::new_text(Some(doc), data.as_bytes()))?;
        returned(made)
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
    #[php(tentative)]
    fn getElementById(
        req: &mut Request,
        this: This<Self>,
        elementId: PhpString,
    ) -> Result<Owner<Raw<Option<Instance<DOMElement>>>>, Thrown> {
        let found = get_element_by_id(req, this.object(), elementId.as_bytes())?;
        returned(found)
    }

    #[php(tentative, implementation = DOMElement::getElementsByTagName)]
    fn getElementsByTagName(
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Instance<crate::map::DOMNodeList>;
    #[php(tentative, implementation = DOMElement::getElementsByTagNameNS)]
    fn getElementsByTagNameNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Instance<crate::map::DOMNodeList>;

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
    fn importNode(
        req: &mut Request,
        this: This<Self>,
        node: Instance<DOMNode>,
        #[php(default = false)] deep: bool,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        import_node(req, this.object(), node.object(), deep).map(Untyped)
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
    #[php(tentative)]
    fn load(
        req: &mut Request,
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] options: i64,
    ) -> Result<bool, Thrown> {
        parse_document(req, this.object(), filename.as_bytes(), options, true)
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
    #[php(tentative)]
    fn loadXML(
        req: &mut Request,
        this: This<Self>,
        source: PhpString,
        #[php(default = 0)] options: i64,
    ) -> Result<bool, Thrown> {
        parse_document(req, this.object(), source.as_bytes(), options, false)
    }

    /// `DOMDocument::normalizeDocument(): void`.
    #[php(tentative)]
    fn normalizeDocument(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        crate::normalize(fetch(req, this.object())?);
        Ok(())
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
    #[php(tentative)]
    fn registerNodeClass(
        req: &mut Request,
        this: This<Self>,
        baseClass: PhpString,
        extendedClass: Option<PhpString>,
    ) -> Result<True, Thrown> {
        let extended = extendedClass.as_ref().map(PhpString::as_bytes);
        register_node_class_as(req, this.object(), baseClass.as_bytes(), extended, false)?;
        Ok(True)
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
    ///
    /// `Dom\XMLDocument::saveXmlFile` aliases it: a `Dom\` document writes
    /// through php's streams and its own XML serialiser.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a/></r>');
    /// $f = tempnam(sys_get_temp_dir(), 'dom');
    /// var_dump($d->saveXmlFile($f), file_get_contents($f));
    /// unlink($f);
    /// var_dump(@$d->saveXmlFile(sys_get_temp_dir() . '/no/such/dir/x.xml'));
    /// ```
    /// ```expect
    /// int(50)
    /// string(50) "<?xml version="1.0" encoding="UTF-8"?>
    /// <r><a/></r>"
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn save(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
        #[php(default = 0)] options: i64,
    ) -> Result<OrFalse<i64>, Thrown> {
        let file = filename.0.as_bytes();
        if file.is_empty() {
            return Err(must_not_be_empty(req, 1));
        }
        let this = this.object();
        let doc = fetch(req, this)?;
        let format = props_of(req, this).format;
        let no_empty = options & i64::from(php_ext_libxml::NOEMPTYTAG) != 0;
        let encoding = doc.doc_encoding();
        let handlers = handlers_of(req, this);
        let bytes =
            handlers.dump_doc_to_file(req, file, doc, format, no_empty, encoding.as_deref())?;
        Ok(if bytes == -1 {
            OrFalse::False
        } else {
            OrFalse::Value(bytes)
        })
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
    #[php(tentative)]
    fn loadHTML(
        req: &mut Request,
        this: This<Self>,
        source: PhpString,
        #[php(default = 0)] options: i64,
    ) -> Result<bool, Thrown> {
        load_html_as(req, this.object(), source.as_bytes(), options, false)
    }

    /// `DOMDocument::loadHTMLFile(string $filename, int $options = 0): bool`.
    #[php(tentative)]
    fn loadHTMLFile(
        req: &mut Request,
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] options: i64,
    ) -> Result<bool, Thrown> {
        load_html_as(req, this.object(), filename.as_bytes(), options, true)
    }

    /// `DOMDocument::saveHTML(?DOMNode $node = null): string|false`.
    #[php(tentative)]
    fn saveHTML(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] node: Option<Instance<DOMNode>>,
    ) -> Result<OrFalse<Owner<Raw<PhpString>>>, Thrown> {
        let saved = save_html(req, this.object(), node.map(|n| n.object()))?;
        or_false(req, saved)
    }

    /// `DOMDocument::saveHTMLFile(string $filename): int|false`.
    #[php(tentative)]
    fn saveHTMLFile(
        req: &mut Request,
        this: This<Self>,
        filename: Path,
    ) -> Result<OrFalse<i64>, Thrown> {
        let file = filename.0.as_bytes();
        if file.is_empty() {
            return Err(must_not_be_empty(req, 1));
        }
        let this = this.object();
        let doc = fetch(req, this)?;
        let format = props_of(req, this).format;
        let encoding = Save::meta_encoding(doc);
        let (bytes, raised, thrown) = Save::html_file(req, file, doc, encoding.as_deref(), format);
        Errors::settle(req, raised, thrown)?;
        Ok(if bytes == -1 {
            OrFalse::False
        } else {
            OrFalse::Value(bytes)
        })
    }

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
    ///
    /// A `Dom\` document saves through php's own XML serialiser
    /// (`php_new_dom_dump_*`), in the document's encoding:
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a"><p:x xmlns:p="urn:p" a="&quot;\'"/><y/></r>');
    /// $y = $d->documentElement->lastChild;
    /// echo $d->saveXml(), "\n", $d->saveXml($y), "\n";
    /// echo $d->saveXml(null, LIBXML_NOXMLDECL), "\n", $d->saveXml(null, LIBXML_NOEMPTYTAG), "\n";
    /// $e = Dom\XMLDocument::createEmpty('1.0', 'ISO-8859-1');
    /// $r = $e->createElementNS('urn:x', 'q:r');
    /// $r->append("\u{e9}\u{20ac}");
    /// $e->append($r);
    /// echo bin2hex($e->saveXml()), "\n";
    /// ```
    /// ```expect
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <r xmlns="urn:a"><p:x xmlns:p="urn:p" a="&quot;'"/><y/></r>
    /// <y xmlns="urn:a"/>
    /// <r xmlns="urn:a"><p:x xmlns:p="urn:p" a="&quot;'"/><y/></r>
    /// <?xml version="1.0" encoding="UTF-8"?>
    /// <r xmlns="urn:a"><p:x xmlns:p="urn:p" a="&quot;'"></p:x><y></y></r>
    /// 3c3f786d6c2076657273696f6e3d22312e302220656e636f64696e673d2249534f2d383835392d31223f3e0a3c713a7220786d6c6e733a713d2275726e3a78223ee92623383336343b3c2f713a723e
    /// ```
    #[php(tentative)]
    fn saveXML(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] node: Option<Instance<DOMNode>>,
        #[php(default = 0)] options: i64,
    ) -> Result<OrFalse<Owner<Raw<PhpString>>>, Thrown> {
        let saved = save_xml(req, this.object(), node.map(|n| n.object()), options)?;
        or_false(req, saved)
    }

    /// `DOMDocument::schemaValidate(string $filename, int $flags = 0): bool`.
    #[php(tentative)]
    fn schemaValidate(
        req: &mut Request,
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> Result<bool, Thrown> {
        let given = (filename.as_bytes(), flags);
        schema_validate_as(req, this.object(), given, Schema::Xsd, true)
    }

    /// `DOMDocument::schemaValidateSource(string $source, int $flags = 0): bool`,
    /// which `Dom\Document` aliases; a modern document's declarations lent to
    /// the validator.
    ///
    /// ```php
    /// $xml = '<root xmlns="urn:test" xmlns:ref="urn:other"><item target="ref:something"/></root>';
    /// $xsd = '<schema xmlns="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:test" elementFormDefault="qualified"><element name="root"><complexType><sequence><element name="item"><complexType><attribute name="target" type="QName"/></complexType></element></sequence></complexType></element></schema>';
    /// $d = Dom\XMLDocument::createFromString($xml);
    /// var_dump($d->schemaValidateSource($xsd), $d->documentElement->getAttributeNames());
    /// ```
    /// ```expect
    /// bool(true)
    /// array(2) {
    ///   [0]=>
    ///   string(5) "xmlns"
    ///   [1]=>
    ///   string(9) "xmlns:ref"
    /// }
    /// ```
    #[php(tentative)]
    fn schemaValidateSource(
        req: &mut Request,
        this: This<Self>,
        source: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> Result<bool, Thrown> {
        let given = (source.as_bytes(), flags);
        schema_validate_as(req, this.object(), given, Schema::Xsd, false)
    }

    /// `DOMDocument::relaxNGValidate(string $filename): bool`.
    #[php(tentative)]
    fn relaxNGValidate(
        req: &mut Request,
        this: This<Self>,
        filename: PhpString,
    ) -> Result<bool, Thrown> {
        let given = (filename.as_bytes(), 0);
        schema_validate_as(req, this.object(), given, Schema::RelaxNg, true)
    }

    /// `DOMDocument::relaxNGValidateSource(string $source): bool`.
    #[php(tentative)]
    fn relaxNGValidateSource(
        req: &mut Request,
        this: This<Self>,
        source: PhpString,
    ) -> Result<bool, Thrown> {
        let given = (source.as_bytes(), 0);
        schema_validate_as(req, this.object(), given, Schema::RelaxNg, false)
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
    #[php(tentative)]
    fn validate(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        let doc = fetch(req, this)?;
        let guard = ns_guard(req, this, doc);
        let (valid, raised, thrown) = Validate::dtd(req, doc);
        drop(guard);
        Errors::settle(req, raised, thrown)?;
        Ok(valid)
    }

    /// `DOMDocument::xinclude(int $options = 0): int|false`: an include
    /// that cannot be read is warned of in the words of the parser libxml opened for
    /// it, and its fallback stands in its place.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="nope.xml"><xi:fallback><f/></xi:fallback></xi:include></r>');
    /// var_dump($d->xinclude());
    /// echo $d->saveXML($d->documentElement), "\n";
    /// ```
    /// ```expect
    /// Warning: DOMDocument::xinclude(): I/O warning : failed to load external entity "%s/nope.xml" in %s on line 4
    /// int(1)
    /// <r xmlns:xi="http://www.w3.org/2001/XInclude"><f/></r>
    /// ```
    #[php(tentative)]
    fn xinclude(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] options: i64,
    ) -> Result<OrFalse<i64>, Thrown> {
        let Ok(flags) = i32::try_from(options) else {
            docref(req, Level::Warning, "Invalid flags");
            return Ok(OrFalse::False);
        };
        let err = run_xinclude(req, this.object(), flags)?;
        Ok(if err == 0 {
            OrFalse::False
        } else {
            OrFalse::Value(i64::from(err))
        })
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
    #[php(tentative)]
    fn adoptNode(
        req: &mut Request,
        this: This<Self>,
        node: Instance<DOMNode>,
    ) -> Result<OrFalse<Owner<Raw<Instance<DOMNode>>>>, Thrown> {
        let adopted = adopt_node(req, this.object(), node.object(), false)?;
        or_false(req, adopted)
    }

    #[php(implementation = DOMElement::append)]
    fn append(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::prepend)]
    fn prepend(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::replaceChildren)]
    fn replaceChildren(this: This<Self>, nodes: Variadic<Untyped<Value>>);
}

/// `abstract class Dom\Document extends Dom\Node implements
/// Dom\ParentNode`: the standard's document, whose clone drops the
/// cached `$children` ([`crate::object::PARENT_HANDLERS`]); its
/// `$children` and `$implementation` keep their slots.
#[php_class(
    namespace(Dom),
    abstract,
    extends(crate::modern::Node),
    implements(crate::parent::ParentNode),
    handlers = crate::object::PARENT_HANDLERS
)]
pub(crate) struct Document;

#[php_class]
impl Document {
    #[php(property)]
    const children: Instance<crate::map::HTMLCollection>;
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const childElementCount: i64;
    #[php(property)]
    const implementation: Instance<crate::classes::Implementation>;
    #[php(property, virtual)]
    const URL: PhpString;
    #[php(property, virtual)]
    const documentURI: PhpString;
    #[php(property, virtual)]
    const characterSet: PhpString;
    #[php(property, virtual)]
    const charset: PhpString;
    #[php(property, virtual)]
    const inputEncoding: PhpString;
    #[php(property, virtual)]
    const doctype: Option<Instance<crate::classes::DocumentType>>;
    #[php(property, virtual)]
    const documentElement: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const body: Option<Instance<crate::element::HTMLElement>>;
    #[php(property, virtual)]
    const head: Option<Instance<crate::element::HTMLElement>>;
    #[php(property, virtual)]
    const title: PhpString;

    #[php(implementation = crate::element::Element::getElementsByTagName)]
    fn getElementsByTagName(
        this: This<Self>,
        qualifiedName: PhpString,
    ) -> Instance<crate::map::HTMLCollection>;
    #[php(implementation = crate::element::Element::getElementsByTagNameNS)]
    fn getElementsByTagNameNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        localName: PhpString,
    ) -> Instance<crate::map::HTMLCollection>;
    #[php(implementation = crate::element::Element::getElementsByClassName)]
    fn getElementsByClassName(
        this: This<Self>,
        classNames: PhpString,
    ) -> Instance<crate::map::HTMLCollection>;

    /// `Dom\Document::createElement(string $localName): Dom\Element`: an HTML
    /// document's element lowercased in the HTML namespace, an XML document's
    /// in none.
    ///
    /// ```php
    /// $x = Dom\XMLDocument::createEmpty();
    /// $e = $x->createElement('Foo');
    /// var_dump(get_class($e), $e->tagName, $e->namespaceURI);
    /// $h = Dom\HTMLDocument::createEmpty();
    /// $e = $h->createElement('Foo');
    /// var_dump(get_class($e), $e->tagName, $e->localName, $e->namespaceURI);
    /// try { $x->createElement('1'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(11) "Dom\Element"
    /// string(3) "Foo"
    /// NULL
    /// string(15) "Dom\HTMLElement"
    /// string(3) "FOO"
    /// string(3) "foo"
    /// string(28) "http://www.w3.org/1999/xhtml"
    /// Invalid Character Error
    /// ```
    fn createElement(
        req: &mut Request,
        this: This<Self>,
        localName: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::element::Element>>>, Thrown> {
        let name = localName.as_bytes();
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(name) {
            return Err(crate::dom_throw(req, Code::InvalidCharacter));
        }
        let node = if doc.kind() == Kind::HtmlDocument {
            let ns = crate::private::mapped(req, this, |mapper, req| mapper.html(req, Some(doc)))?;
            Node::new_element(Some(doc), ns, &name.to_ascii_lowercase(), None)
        } else {
            Node::new_element(Some(doc), None, name, None)
        };
        let made = created(req, this, node)?;
        returned(made)
    }

    /// `Dom\Document::createElementNS(?string $namespace, string $qualifiedName): Dom\Element`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createEmpty();
    /// $e = $d->createElementNS('urn:a', 'p:e');
    /// var_dump($e->tagName, $e->prefix, $e->localName, $e->namespaceURI);
    /// $f = $d->createElementNS(null, 'g');
    /// var_dump($f->namespaceURI, $f->prefix);
    /// $h = $d->createElementNS('http://www.w3.org/1999/xhtml', 'div');
    /// var_dump(get_class($h));
    /// try { $d->createElementNS(null, 'p:x'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// try { $d->createElementNS('urn:a', '1'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// try { $d->createElementNS('urn:a', 'xmlns:x'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(3) "p:e"
    /// string(1) "p"
    /// string(1) "e"
    /// string(5) "urn:a"
    /// NULL
    /// NULL
    /// string(15) "Dom\HTMLElement"
    /// Namespace Error
    /// Invalid Character Error
    /// Namespace Error
    /// ```
    fn createElementNS(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::element::Element>>>, Thrown> {
        let uri = namespace.as_ref().map(PhpString::as_bytes);
        let this = this.object();
        let doc = fetch(req, this)?;
        let made = match validate_and_extract(uri, qualifiedName.as_bytes()) {
            Ok((local, prefix)) => {
                let ns =
                    crate::element::mapped_ns(req, this, Some(doc), prefix.as_deref(), namespace)?;
                match Node::new_element(Some(doc), ns, &local, None) {
                    Some(node) => object_for(req, node, Some(this))?,
                    None => return Err(crate::dom_throw(req, Code::InvalidState)),
                }
            }
            Err(code) => {
                let strict = strict(req, this);
                dom_error(req, code, strict)?;
                Owner::null()
            }
        };
        returned(made)
    }

    #[php(implementation = DOMDocument::createDocumentFragment)]
    fn createDocumentFragment(this: This<Self>) -> Instance<crate::classes::DocumentFragment>;
    #[php(implementation = DOMDocument::createTextNode)]
    fn createTextNode(this: This<Self>, data: PhpString) -> Instance<crate::classes::Text>;
    #[php(implementation = DOMDocument::createCDATASection)]
    fn createCDATASection(
        this: This<Self>,
        data: PhpString,
    ) -> Instance<crate::classes::CDATASection>;
    #[php(implementation = DOMDocument::createComment)]
    fn createComment(this: This<Self>, data: PhpString) -> Instance<crate::classes::Comment>;

    /// `Dom\Document::createProcessingInstruction(string $target, string $data): Dom\ProcessingInstruction`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createEmpty();
    /// $p = $d->createProcessingInstruction('t', 'a ? > b');
    /// var_dump(get_class($p), $p->target, $p->data);
    /// try { $d->createProcessingInstruction('t', 'a?>b'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $d->createProcessingInstruction('1', 'x'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(25) "Dom\ProcessingInstruction"
    /// string(1) "t"
    /// string(7) "a ? > b"
    /// Invalid character sequence "?>" in processing instruction
    /// Invalid Character Error
    /// ```
    fn createProcessingInstruction(
        req: &mut Request,
        this: This<Self>,
        target: PhpString,
        data: PhpString,
    ) -> Result<Owner<Raw<Instance<crate::classes::ProcessingInstruction>>>, Thrown> {
        let (target, data) = (target.as_bytes(), data.as_bytes());
        let this = this.object();
        let doc = fetch(req, this)?;
        if !Name::valid(target) {
            let refused = invalid_name(req, this)?;
            return returned(refused);
        }
        if data.windows(2).any(|w| w == b"?>") {
            return Err(crate::dom_throw_message(
                req,
                Code::InvalidCharacter,
                "Invalid character sequence \"?>\" in processing instruction",
            ));
        }
        let made = created(req, this, Node::new_pi(Some(doc), target, Some(data)))?;
        returned(made)
    }

    /// `Dom\Document::importNode(?Dom\Node $node, bool $deep = false): Dom\Node`.
    ///
    /// ```php
    /// $a = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><p:c x="1">t</p:c></r>');
    /// $b = Dom\XMLDocument::createFromString('<s/>');
    /// $r = $a->documentElement;
    /// $c = $r->firstChild;
    /// $n = $b->importNode($c, true);
    /// $shallow = $b->importNode($c);
    /// var_dump($n->ownerDocument === $b, $n->parentNode, $n->namespaceURI, $n->getAttribute('x'), $n->textContent, $shallow->hasChildNodes());
    /// $s = $b->documentElement;
    /// var_dump($b->importNode($s) === $s);
    /// try { $b->importNode($a); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// NULL
    /// string(5) "urn:p"
    /// string(1) "1"
    /// string(1) "t"
    /// bool(false)
    /// bool(true)
    /// Not Supported Error
    /// ```
    fn importNode(
        req: &mut Request,
        this: This<Self>,
        node: Option<Instance<crate::modern::Node>>,
        #[php(default = false)] deep: bool,
    ) -> Result<Owner<Raw<Instance<crate::modern::Node>>>, Thrown> {
        let imported = modern_import(req, this.object(), node.map(|n| n.object()), deep)?;
        returned(imported)
    }

    /// `Dom\Document::adoptNode(Dom\Node $node): Dom\Node`: the node's
    /// namespaces moved to this document's mapper; a failed adoption an
    /// `Invalid State Error`.
    ///
    /// ```php
    /// $a = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><p:c q="1"/></r>');
    /// $b = Dom\XMLDocument::createFromString('<s/>');
    /// $r = $a->documentElement;
    /// $c = $r->firstChild;
    /// var_dump($b->adoptNode($c) === $c, $c->ownerDocument === $b, $r->hasChildNodes(), $c->namespaceURI, $c->parentNode);
    /// $s = $b->documentElement;
    /// $s->append($c);
    /// var_dump($s->firstChild === $c, $c->getAttribute('q'));
    /// try { $b->adoptNode($a); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// string(5) "urn:p"
    /// NULL
    /// bool(true)
    /// string(1) "1"
    /// Not Supported Error
    /// ```
    fn adoptNode(
        req: &mut Request,
        this: This<Self>,
        node: Instance<crate::modern::Node>,
    ) -> Result<Owner<Raw<Instance<crate::modern::Node>>>, Thrown> {
        let adopted = adopt_node(req, this.object(), node.object(), true)?;
        returned(adopted)
    }

    #[php(implementation = DOMDocument::createAttribute)]
    fn createAttribute(this: This<Self>, localName: PhpString) -> Instance<crate::modern::Attr>;
    #[php(implementation = DOMDocument::createAttributeNS)]
    fn createAttributeNS(
        this: This<Self>,
        namespace: Option<PhpString>,
        qualifiedName: PhpString,
    ) -> Instance<crate::modern::Attr>;
    #[php(implementation = DOMDocument::getElementById)]
    fn getElementById(
        this: This<Self>,
        elementId: PhpString,
    ) -> Option<Instance<crate::element::Element>>;

    /// `Dom\Document::registerNodeClass(string $baseClass, ?string $extendedClass): void`:
    /// the base a `Dom\Node` class.
    ///
    /// ```php
    /// class E extends Dom\Element {}
    /// $d = Dom\XMLDocument::createFromString('<r/>');
    /// var_dump($d->registerNodeClass('Dom\Element', 'E'));
    /// echo get_class($d->documentElement), ' ', get_class($d->createElement('x')), "\n";
    /// $d->registerNodeClass('Dom\Element', null);
    /// echo get_class($d->createElement('y')), "\n";
    /// try { $d->registerNodeClass('DOMElement', null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $d->registerNodeClass('Dom\Element', 'Dom\Attr'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// NULL
    /// E E
    /// Dom\Element
    /// Dom\Document::registerNodeClass(): Argument #1 ($baseClass) must be a class name derived from Dom\Node, DOMElement given
    /// Dom\Document::registerNodeClass(): Argument #2 ($extendedClass) must be a class name derived from Dom\Element or null, Dom\Attr given
    /// ```
    fn registerNodeClass(
        req: &mut Request,
        this: This<Self>,
        baseClass: PhpString,
        extendedClass: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let extended = extendedClass.as_ref().map(PhpString::as_bytes);
        register_node_class_as(req, this.object(), baseClass.as_bytes(), extended, true)
    }

    #[php(implementation = DOMDocument::schemaValidate)]
    fn schemaValidate(
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> bool;
    #[php(implementation = DOMDocument::schemaValidateSource)]
    fn schemaValidateSource(
        this: This<Self>,
        source: PhpString,
        #[php(default = 0)] flags: i64,
    ) -> bool;
    #[php(implementation = DOMDocument::relaxNGValidate)]
    fn relaxNgValidate(this: This<Self>, filename: PhpString) -> bool;
    #[php(implementation = DOMDocument::relaxNGValidateSource)]
    fn relaxNgValidateSource(this: This<Self>, source: PhpString) -> bool;
    #[php(implementation = DOMElement::append)]
    fn append(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::prepend)]
    fn prepend(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceChildren)]
    fn replaceChildren(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );

    /// `Dom\Document::importLegacyNode(DOMNode $node, bool $deep = false): Dom\Node`.
    ///
    /// ```php
    /// $old = new DOMDocument;
    /// $old->loadXML('<r xmlns="urn:d" xmlns:p="urn:p"><p:c a="1"/></r>');
    /// $d = Dom\XMLDocument::createEmpty();
    /// $root = $old->documentElement;
    /// $n = $d->importLegacyNode($root, true);
    /// $c = $n->firstChild;
    /// var_dump(get_class($n), $n->namespaceURI, $c->namespaceURI, $n->getAttributeNames());
    /// ```
    /// ```expect
    /// string(11) "Dom\Element"
    /// string(5) "urn:d"
    /// string(5) "urn:p"
    /// array(2) {
    ///   [0]=>
    ///   string(5) "xmlns"
    ///   [1]=>
    ///   string(7) "xmlns:p"
    /// }
    /// ```
    fn importLegacyNode(
        req: &mut Request,
        this: This<Self>,
        node: Instance<DOMNode>,
        #[php(default = false)] deep: bool,
    ) -> Result<Owner<Raw<Instance<crate::modern::Node>>>, Thrown> {
        let imported = modern_import(req, this.object(), Some(node.object()), deep)?;
        returned(imported)
    }

    #[php(implementation = crate::element::Element::querySelector)]
    fn querySelector(
        this: This<Self>,
        selectors: PhpString,
    ) -> Option<Instance<crate::element::Element>>;
    #[php(implementation = crate::element::Element::querySelectorAll)]
    fn querySelectorAll(this: This<Self>, selectors: PhpString) -> Instance<crate::map::NodeList>;
}

/// `final class Dom\XMLDocument extends Dom\Document`: the factories of
/// `xml_document.c`, and `DOMDocument`'s savers and validators.
///
/// A document costs what php's does, its `php_dom_private_data` among it,
/// and its unset gives all of it back:
///
/// ```php
/// $w = Dom\XMLDocument::createEmpty(); unset($w);
/// $m = memory_get_usage(); $d = Dom\XMLDocument::createEmpty(); var_dump(memory_get_usage() - $m);
/// $m = memory_get_usage(); unset($d); var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(264)
/// int(-264)
/// ```
#[php_class(
    namespace(Dom),
    final,
    extends(Document),
    handlers = crate::object::NODE_HANDLERS
)]
pub(crate) struct XMLDocument;

#[php_class]
impl XMLDocument {
    #[php(property, virtual)]
    const xmlEncoding: PhpString;
    #[php(property, virtual)]
    const xmlStandalone: bool;
    #[php(property, virtual)]
    const xmlVersion: PhpString;
    #[php(property, virtual)]
    const formatOutput: bool;

    /// `Dom\XMLDocument::createEmpty(string $version = "1.0", string $encoding = "UTF-8"): Dom\XMLDocument`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createEmpty();
    /// var_dump(get_class($d), $d->xmlVersion, $d->xmlEncoding, $d->childNodes->length);
    /// $d = Dom\XMLDocument::createEmpty('1.1', 'ISO-8859-1');
    /// var_dump($d->xmlVersion, $d->characterSet);
    /// try { Dom\XMLDocument::createEmpty('1.0', 'nope'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(15) "Dom\XMLDocument"
    /// string(3) "1.0"
    /// string(5) "UTF-8"
    /// int(0)
    /// string(3) "1.1"
    /// string(10) "ISO-8859-1"
    /// Dom\XMLDocument::createEmpty(): Argument #2 ($encoding) is not a valid document encoding
    /// ```
    fn createEmpty(
        req: &mut Request,
        #[php(default = "1.0")] version: PhpString,
        #[php(default = "UTF-8")] encoding: Path,
    ) -> Result<Owner<Instance<XMLDocument>>, Thrown> {
        let encoding = encoding.0.as_bytes();
        if !Library::known_encoding(encoding) {
            return Err(php_ext::raising::argument_error(
                req,
                "ValueError",
                2,
                "is not a valid document encoding",
            ));
        }
        let Some(doc) = Node::new_doc(Some(version.as_bytes())) else {
            return Err(crate::dom_throw(req, Code::InvalidState));
        };
        doc.set_doc_string(DocString::Encoding, Some(encoding));
        let private = crate::private::Private::new(req)?;
        let made = crate::modern::document_object(req, "Dom\\XMLDocument", doc, private)?;
        Ok(made.into_instance())
    }

    /// `Dom\XMLDocument::createFromFile(string $path, int $options = 0, ?string $overrideEncoding = null): Dom\XMLDocument`.
    ///
    /// ```php
    /// $f = tempnam(sys_get_temp_dir(), 'dom');
    /// file_put_contents($f, '<r xmlns:p="urn:p"><p:a/></r>');
    /// $d = Dom\XMLDocument::createFromFile($f);
    /// var_dump($d->documentElement->firstChild->namespaceURI, str_starts_with($d->URL, 'file://'), $d->URL === 'file://' . realpath($f));
    /// unlink($f);
    /// try { Dom\XMLDocument::createFromFile(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// try { Dom\XMLDocument::createFromFile('a%00b'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(5) "urn:p"
    /// bool(true)
    /// bool(true)
    /// Dom\XMLDocument::createFromFile(): Argument #1 ($path) must not be empty
    /// Dom\XMLDocument::createFromFile(): Argument #1 ($path) must not contain percent-encoded NUL bytes
    /// ```
    fn createFromFile(
        req: &mut Request,
        path: PhpString,
        #[php(default = 0)] options: i64,
        #[php(default = null)] overrideEncoding: Option<PhpString>,
    ) -> Result<Owner<Instance<XMLDocument>>, Thrown> {
        crate::modern::xml_load(req, (path, options, overrideEncoding), true)
            .map(Owner::into_instance)
    }

    /// `Dom\XMLDocument::createFromString(string $source, int $options = 0, ?string $overrideEncoding = null): Dom\XMLDocument`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a" xmlns:p="urn:p"><p:c/></r>');
    /// $r = $d->documentElement;
    /// var_dump(get_class($r), $r->namespaceURI, $r->attributes->length, $r->firstChild->prefix, $r->firstChild->namespaceURI);
    /// try { Dom\XMLDocument::createFromString('<a>'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { Dom\XMLDocument::createFromString(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(11) "Dom\Element"
    /// string(5) "urn:a"
    /// int(2)
    /// string(1) "p"
    /// string(5) "urn:p"
    ///
    /// Warning: Dom\XMLDocument::createFromString(): Premature end of data in tag a line 1 in Entity, line: 1 in %s on line 5
    /// XML fragment is not well-formed
    /// Dom\XMLDocument::createFromString(): Argument #1 ($source) must not be empty
    /// ```
    fn createFromString(
        req: &mut Request,
        source: PhpString,
        #[php(default = 0)] options: i64,
        #[php(default = null)] overrideEncoding: Option<PhpString>,
    ) -> Result<Owner<Instance<XMLDocument>>, Thrown> {
        crate::modern::xml_load(req, (source, options, overrideEncoding), false)
            .map(Owner::into_instance)
    }

    #[php(implementation = DOMDocument::createEntityReference)]
    fn createEntityReference(
        this: This<Self>,
        name: PhpString,
    ) -> Instance<crate::classes::EntityReference>;
    #[php(implementation = DOMDocument::validate)]
    fn validate(this: This<Self>) -> bool;

    /// `Dom\XMLDocument::xinclude(int $options = 0): int`: flags past an
    /// `int` a `ValueError`, a failed substitution an `Invalid Modification
    /// Error`.
    ///
    /// ```php
    /// $f = tempnam(sys_get_temp_dir(), 'dom');
    /// file_put_contents($f, '<x/>');
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="' . $f . '"/></r>');
    /// var_dump($d->xinclude());
    /// $r = $d->documentElement;
    /// var_dump($r->firstChild->tagName);
    /// unlink($f);
    /// try { $d->xinclude(PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// string(1) "x"
    /// Dom\XMLDocument::xinclude(): Argument #1 ($options) is too large
    /// ```
    fn xinclude(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] options: i64,
    ) -> Result<i64, Thrown> {
        let Ok(flags) = i32::try_from(options) else {
            return Err(php_ext::raising::argument_error(
                req,
                "ValueError",
                1,
                "is too large",
            ));
        };
        let err = run_xinclude(req, this.object(), flags)?;
        if err < 0 {
            return Err(crate::dom_throw(req, Code::InvalidModification));
        }
        Ok(i64::from(err))
    }

    #[php(implementation = DOMDocument::saveXML)]
    fn saveXml(
        this: This<Self>,
        #[php(default = null)] node: Option<Instance<crate::modern::Node>>,
        #[php(default = 0)] options: i64,
    ) -> OrFalse<PhpString>;
    #[php(implementation = DOMDocument::save)]
    fn saveXmlFile(
        this: This<Self>,
        filename: PhpString,
        #[php(default = 0)] options: i64,
    ) -> OrFalse<i64>;
}

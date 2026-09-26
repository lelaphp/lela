//! What the `Dom\` classes add to the object model of `php_dom.c`: the
//! private constructor every `Dom\Node` has, the clone handlers that drop
//! a cached `[SameObject]` property, the document objects the factories
//! of `xml_document.c` and `html_document.c` hand out, and the cached
//! objects php keeps in a declared property's slot.

use php_ext::abi::{ObjectHandlers, PropertyTest, Std};
use php_ext::raising::{Thrown, arg_value_error, throw};
use php_ext::request::Request;
use php_ext::values::{PhpObject, PhpString, Value};
use php_ext_libxml::ffi::{ClassType, DocString, Library, Node, Parsed, Parser, Source};
use php_ext_libxml::{Errors, ffi as f};

use crate::object::Code;
use crate::private::Private;
use crate::{
    Entry, arity, bytes_arg, dom, dom_mut, dom_throw, long_arg, node_of, nullable_bytes_arg,
};

/// `php_dom_follow_spec_intern`: whether the object's document hands out
/// the `Dom\` classes.
///
/// ```php
/// $l = new DOMDocument; $l->loadXML('<r/>');
/// $m = Dom\XMLDocument::createFromString('<r/>');
/// var_dump($l->documentElement->nodeValue, $m->documentElement->nodeValue, get_class($m->documentElement));
/// ```
/// ```expect
/// string(0) ""
/// NULL
/// string(11) "Dom\Element"
/// ```
pub(crate) fn follows_spec(req: &Request, object: PhpObject) -> bool {
    dom(req, object).is_some_and(|d| d.obj.class_type() == ClassType::Modern)
}

/// `Dom\Node::__construct()`: private and final, so a program reaches it
/// through Reflection alone, and is refused; `Dom\TokenList` and
/// `Dom\NamespaceInfo` alias it.
///
/// ```php
/// try { new Dom\Element(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $doc = Dom\XMLDocument::createEmpty();
/// try { (new ReflectionMethod($doc, '__construct'))->invoke($doc); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Call to private Dom\Node::__construct() from global scope
/// Cannot directly construct Dom\XMLDocument, use document methods instead
/// ```
fn node_construct(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    let class = crate::class_name(req, this.class());
    Err(throw(
        req,
        "Error",
        &format!("Cannot directly construct {class}, use document methods instead"),
    ))
}

/// A fresh object of `class` standing for `doc`, a document nothing counts
/// yet, pinned to the `Dom\` classes with its private data laid down:
/// `php_dom_instantiate_object_helper`, `dom_set_xml_class` and
/// `php_dom_private_data_create`.
///
/// ```php
/// $d = Dom\XMLDocument::createEmpty();
/// var_dump(get_class($d), $d->ownerDocument, $d->createElement('e')->ownerDocument === $d);
/// ```
/// ```expect
/// string(15) "Dom\XMLDocument"
/// NULL
/// bool(true)
/// ```
pub(crate) fn document_object(
    req: &mut Request,
    class: &str,
    doc: Node,
) -> Result<PhpObject, Thrown> {
    let class = crate::class(req, class)?;
    let object = php_ext::call::new_object(req, class)?;
    if let Some(d) = dom_mut(req, object) {
        d.obj.set_object(object);
        d.obj.adopt_document(doc);
        d.obj.take_node(doc);
        d.obj.set_class_type(ClassType::Modern);
        d.obj.set_private_data(Box::new(Private::default()));
    }
    Ok(object)
}

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
fn xml_create_empty(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 2)?;
    let version = match crate::arg(req, 1)? {
        Some(v) => v
            .as_str_bytes()
            .map_or_else(|| b"1.0".to_vec(), <[u8]>::to_vec),
        None => b"1.0".to_vec(),
    };
    let encoding = match crate::arg(req, 2)? {
        Some(_) => crate::path_arg(req, 2)?,
        None => b"UTF-8".to_vec(),
    };
    if !Library::known_encoding(&encoding) {
        return Err(php_ext::raising::argument_error(
            req,
            "ValueError",
            2,
            "is not a valid document encoding",
        ));
    }
    let Some(doc) = Node::new_doc(Some(&version)) else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    doc.set_doc_string(DocString::Encoding, Some(&encoding));
    Ok(document_object(req, "Dom\\XMLDocument", doc)?.as_value())
}

/// `check_options_validity` of `xml_document.c`: the flags the modern XML
/// parser takes, `LIBXML_NO_XXE` not among them before libxml 2.13.
const XML_OPTIONS: i64 = (f::XML_PARSE_RECOVER
    | f::XML_PARSE_NOENT
    | f::XML_PARSE_DTDLOAD
    | f::XML_PARSE_DTDATTR
    | f::XML_PARSE_DTDVALID
    | f::XML_PARSE_NOERROR
    | f::XML_PARSE_NOWARNING
    | f::XML_PARSE_NOBLANKS
    | f::XML_PARSE_XINCLUDE
    | f::XML_PARSE_NSCLEAN
    | f::XML_PARSE_NOCDATA
    | f::XML_PARSE_NONET
    | f::XML_PARSE_PEDANTIC
    | f::XML_PARSE_COMPACT
    | f::XML_PARSE_HUGE
    | f::XML_PARSE_BIG_LINES) as i64;

/// `load_from_helper`: `Dom\XMLDocument::createFromString` and
/// `createFromFile`.
/// `load_from_helper`'s parameters held to php's checks.
struct LoadArgs {
    source: Vec<u8>,
    options: i32,
    override_encoding: Option<Vec<u8>>,
}

fn xml_load_args(req: &mut Request, from_file: bool) -> Result<LoadArgs, Thrown> {
    arity(req, 1, 3)?;
    let source = bytes_arg(req, 1)?;
    let options = long_arg(req, 2, 0)?;
    let override_encoding = nullable_bytes_arg(req, 3)?;
    if from_file && source.contains(&0) {
        return Err(arg_value_error(req, 1, "not contain any null bytes"));
    }
    if override_encoding.as_ref().is_some_and(|e| e.contains(&0)) {
        return Err(arg_value_error(req, 3, "not contain any null bytes"));
    }
    if source.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if i32::try_from(source.len()).is_err() {
        return Err(php_ext::raising::argument_error(
            req,
            "ValueError",
            1,
            "is too long",
        ));
    }
    if from_file && source.windows(3).any(|w| w == b"%00") {
        return Err(arg_value_error(
            req,
            1,
            "not contain percent-encoded NUL bytes",
        ));
    }
    if options & !XML_OPTIONS != 0 {
        return Err(php_ext::raising::argument_error(
            req,
            "ValueError",
            2,
            "contains invalid flags (allowed flags: LIBXML_RECOVER, LIBXML_NOENT, LIBXML_DTDLOAD, \
             LIBXML_DTDATTR, LIBXML_DTDVALID, LIBXML_NOERROR, LIBXML_NOWARNING, LIBXML_NOBLANKS, \
             LIBXML_XINCLUDE, LIBXML_NSCLEAN, LIBXML_NOCDATA, LIBXML_NONET, LIBXML_PEDANTIC, \
             LIBXML_COMPACT, LIBXML_PARSEHUGE, LIBXML_BIGLINES)",
        ));
    }
    let mut options = i32::try_from(options).unwrap_or(0);
    if let Some(enc) = &override_encoding {
        if !Library::known_encoding(enc) {
            return Err(arg_value_error(req, 3, "be a valid document encoding"));
        }
        options |= f::XML_PARSE_IGNORE_ENC;
    }
    Ok(LoadArgs {
        source,
        options,
        override_encoding,
    })
}

fn xml_load(req: &mut Request, from_file: bool) -> Result<Value, Thrown> {
    let LoadArgs {
        source,
        options,
        override_encoding,
    } = xml_load_args(req, from_file)?;
    let recover = options & f::XML_PARSE_RECOVER != 0;
    let (parsed, raised, thrown) = if from_file {
        match crate::file_path(req, &source) {
            Some(path) => Parser::xml(
                req,
                Source::File(&path),
                options,
                None,
                recover,
                override_encoding.as_deref(),
            ),
            None => (Parsed::Nothing, Vec::new(), None),
        }
    } else {
        let dir = crate::document::base_directory(req);
        Parser::xml(
            req,
            Source::Memory(&source),
            options,
            Some(&dir),
            recover,
            override_encoding.as_deref(),
        )
    };
    if let Err(thrown) = Errors::settle(req, raised, thrown) {
        if let Parsed::Document(Some(doc)) = parsed {
            doc.free_document();
        }
        return Err(thrown);
    }
    let doc = match parsed {
        Parsed::Document(Some(doc)) => doc,
        Parsed::Malformed => {
            return Err(crate::dom_throw_message(
                req,
                Code::Syntax,
                "XML fragment is not well-formed",
            ));
        }
        Parsed::Nothing | Parsed::Document(None) => {
            if from_file {
                let name = String::from_utf8_lossy(&source).into_owned();
                return Err(throw(
                    req,
                    "Exception",
                    &format!("Cannot open file '{name}'"),
                ));
            }
            return Err(dom_throw(req, Code::InvalidState));
        }
    };
    if doc.doc_encoding().is_none() {
        let encoding = override_encoding.unwrap_or_else(|| b"UTF-8".to_vec());
        doc.set_doc_string(DocString::Encoding, Some(&encoding));
    }
    if from_file
        && let Some(url) = doc.doc_url()
        && !is_stream_path(&url)
        && !url.starts_with(b"file:/")
    {
        let fixed = [b"file://".as_slice(), &url].concat();
        doc.set_doc_string(DocString::Url, Some(&fixed));
    }
    let object = document_object(req, "Dom\\XMLDocument", doc)?;
    convert_to_modern(req, object, doc);
    Ok(object.as_value())
}

/// `dom_document_convert_to_modern`'s walk, `dom_mark_namespaces_as_attributes_too`:
/// every element's declarations become `xmlns` attributes.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:a" xmlns:b="urn:b" c="1"><b:x/></r>');
/// foreach ($d->documentElement->attributes as $a) echo $a->name, '=', $a->value, ' ', var_export($a->namespaceURI, true), "\n";
/// ```
/// ```expect
/// xmlns=urn:a 'http://www.w3.org/2000/xmlns/'
/// xmlns:b=urn:b 'http://www.w3.org/2000/xmlns/'
/// c=1 NULL
/// ```
pub(crate) fn convert_to_modern(req: &mut Request, object: PhpObject, doc: Node) {
    let Some(private) = dom_mut(req, object).and_then(|d| crate::private::of(&mut d.obj)) else {
        return;
    };
    let mut at = doc.children();
    while let Some(node) = at {
        if node.kind() == php_ext_libxml::ffi::Kind::Element {
            private.mapper.mark_attribute_list(node);
        }
        at = crate::next_in_tree_order_from(node, None);
    }
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
fn xml_create_from_string(req: &mut Request) -> Result<Value, Thrown> {
    xml_load(req, false)
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
fn xml_create_from_file(req: &mut Request) -> Result<Value, Thrown> {
    xml_load(req, true)
}

/// `Dom\import_simplexml(object $node): Dom\Attr|Dom\Element`: there is no
/// simplexml, so a modern element or attribute answers as itself.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r a="1"/>');
/// var_dump(Dom\import_simplexml($d->documentElement) === $d->documentElement);
/// try { Dom\import_simplexml(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// Dom\import_simplexml(): Argument #1 ($node) is not a valid node type
/// ```
pub(crate) fn import_simplexml(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let object = crate::object_arg(req, 1)?;
    let node = object
        .filter(|o| crate::instance_of(req, *o, "Dom\\Node"))
        .and_then(|o| node_of(req, o).map(|n| (o, n)));
    match node {
        Some((o, n))
            if matches!(
                n.kind(),
                php_ext_libxml::ffi::Kind::Element | php_ext_libxml::ffi::Kind::Attribute
            ) =>
        {
            crate::object_for(req, n, Some(o))
        }
        _ => Err(php_ext::raising::argument_error(
            req,
            "TypeError",
            1,
            "is not a valid node type",
        )),
    }
}

// ---- the cached `[SameObject]` properties

/// The object php keeps in the declared slot `name` of `object`, `None`
/// where nothing is kept yet.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// var_dump(array_keys(get_object_vars($e)));
/// $e->classList; $e->children;
/// var_dump(array_keys(get_object_vars($e)));
/// $c = clone $e;
/// var_dump(array_keys(get_object_vars($c)), $c->classList === $e->classList);
/// $f = $d->createDocumentFragment(); $f->children;
/// var_dump(array_keys(get_object_vars(clone $f)));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(8) "children"
///   [1]=>
///   string(9) "classList"
/// }
/// array(0) {
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
pub(crate) fn cached(req: &Request, object: PhpObject, name: &[u8]) -> Option<Value> {
    object.prop(req, name)
}

/// The object made for `name` kept in its declared slot, bypassing the
/// class's handlers as `ZVAL_OBJ` into `OBJ_PROP` does.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// var_dump(array_keys(get_object_vars($e)));
/// $e->classList; $e->children;
/// var_dump(array_keys(get_object_vars($e)));
/// $c = clone $e;
/// var_dump(array_keys(get_object_vars($c)), $c->classList === $e->classList);
/// $f = $d->createDocumentFragment(); $f->children;
/// var_dump(array_keys(get_object_vars(clone $f)));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(8) "children"
///   [1]=>
///   string(9) "classList"
/// }
/// array(0) {
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
pub(crate) fn keep(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    value: &Value,
) -> Result<(), Thrown> {
    req.addref(value);
    let key = PhpString::new(req, name)?;
    Std.write_property(req, object, key, *value)
}

/// Forget what the slot `name` keeps, as a clone does with `ZVAL_UNDEF`.
fn forget(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    if Std.has_property(req, object, name, PropertyTest::Exists)? {
        Std.unset_property(req, object, name)?;
    }
    Ok(())
}

/// `dom_modern_element_clone_obj`.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// var_dump(array_keys(get_object_vars($e)));
/// $e->classList; $e->children;
/// var_dump(array_keys(get_object_vars($e)));
/// $c = clone $e;
/// var_dump(array_keys(get_object_vars($c)), $c->classList === $e->classList);
/// $f = $d->createDocumentFragment(); $f->children;
/// var_dump(array_keys(get_object_vars(clone $f)));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(8) "children"
///   [1]=>
///   string(9) "classList"
/// }
/// array(0) {
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
pub(crate) fn clone_element(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let clone = crate::clone_object(req, object)?;
    forget(req, clone, b"classList")?;
    forget(req, clone, b"children")?;
    Ok(clone)
}

/// `dom_clone_obj_unset_children_property`.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $e = $d->createElement('p');
/// var_dump(array_keys(get_object_vars($e)));
/// $e->classList; $e->children;
/// var_dump(array_keys(get_object_vars($e)));
/// $c = clone $e;
/// var_dump(array_keys(get_object_vars($c)), $c->classList === $e->classList);
/// $f = $d->createDocumentFragment(); $f->children;
/// var_dump(array_keys(get_object_vars(clone $f)));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(2) {
///   [0]=>
///   string(8) "children"
///   [1]=>
///   string(9) "classList"
/// }
/// array(0) {
/// }
/// bool(false)
/// array(0) {
/// }
/// ```
pub(crate) fn clone_parent(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let clone = crate::clone_object(req, object)?;
    forget(req, clone, b"children")?;
    Ok(clone)
}

/// `php_is_stream_path`: a `scheme://` path, or `data:`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// file_put_contents($f, "<!doctype html>\n<title>t</title><p>x");
/// $d = Dom\HTMLDocument::createFromFile($f);
/// var_dump($d->title, $d->body->textContent, $d->URL === 'file://' . realpath($f));
/// unlink($f);
/// try { @Dom\HTMLDocument::createFromFile('/nonexistent/x.html'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "t"
/// string(1) "x"
/// bool(true)
/// Cannot open file '/nonexistent/x.html'
/// ```
pub(crate) fn is_stream_path(path: &[u8]) -> bool {
    let n = path
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        .count();
    let rest = path.get(n..).unwrap_or_default();
    (n > 1 && rest.starts_with(b"://"))
        || (n == 4
            && rest.starts_with(b":")
            && path
                .get(..4)
                .is_some_and(|s| s.eq_ignore_ascii_case(b"data")))
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("Dom\\Node::__construct", node_construct),
    ("Dom\\XMLDocument::createEmpty", xml_create_empty),
    ("Dom\\XMLDocument::createFromString", xml_create_from_string),
    ("Dom\\XMLDocument::createFromFile", xml_create_from_file),
];

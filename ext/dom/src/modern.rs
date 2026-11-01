//! What the `Dom\` classes add to the object model of `php_dom.c`: the
//! private constructor every `Dom\Node` has, the clone handlers that drop
//! a cached `[SameObject]` property, the document objects the factories
//! of `xml_document.c` and `html_document.c` hand out, and the cached
//! objects php keeps in a declared property's slot.

use php_ext::abi::{Instance, ObjectHandlers, OrFalse, PropertyTest, Raw, Std, This, Union};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::{Thrown, arg_value_error, throw_error, throw_exception_ex};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, PhpString, Value};
use php_ext_libxml::ffi::{ClassType, DocString, Library, Parsed, Parser, Source};
use php_ext_libxml::{Errors, ffi as f};

use crate::node::DOMNode;
use crate::object::{Code, Dom};
use crate::parent::is_parent_invalid;
use crate::private::Private;
use crate::throwables::Throwables;
use crate::{dom, dom_throw, fetch, invalidate, object_for, returned};

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

/// A fresh object of `class` standing for `doc`, a document nothing counts
/// yet, pinned to the `Dom\` classes with `private` laid down as its
/// private data: `php_dom_instantiate_object_helper`, `dom_set_xml_class`
/// and the `private_data` its caller made with
/// `php_dom_private_data_create`. The one private data a document has
/// is the one handed in, so a caller that filled a mapper keeps it. The
/// `Err` gives `private` and `doc` up, and an object with no libxml half
/// to lay them on gives both up, `doc` freed as it drops; otherwise `doc`
/// takes its first reference ([`f::DocOf::first`]) and no other.
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
    doc: f::NewDoc,
    private: Private,
) -> Result<Owner<PhpObject>, Thrown> {
    let object = match crate::class(req, class).and_then(|c| php_ext::call::new_object(req, c)) {
        Ok(object) => object,
        Err(thrown) => {
            private.free(req);
            return Err(thrown);
        }
    };
    if let Some(mut obj) = crate::libxml_half(req, *object) {
        let node = doc.node();
        obj.count_document(req, f::DocOf::first(doc));
        obj.take_node(req, node);
        obj.set_class_type(ClassType::Modern);
        obj.set_document_handlers(&crate::xml_serializer::MODERN);
        obj.set_private_data(req, Box::new(private));
    } else {
        private.free(req);
    }
    Ok(object)
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

fn xml_load_args(
    req: &mut Request,
    (source, options, override_encoding): (PhpString, i64, Option<PhpString>),
    from_file: bool,
) -> Result<LoadArgs, Thrown> {
    let source = source.as_bytes().to_vec();
    let override_encoding = override_encoding.map(|e| e.as_bytes().to_vec());
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
        let ce = Throwables::of(req)?;
        return Err(php_ext::raising::argument_error(
            req,
            ce.value_error,
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
        let ce = Throwables::of(req)?;
        return Err(php_ext::raising::argument_error(
            req,
            ce.value_error,
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

/// The body `Dom\XMLDocument::createFromString` and `createFromFile`
/// share. A file that does not open is php's `Cannot open file '%s'`,
/// the name's bytes as given:
///
/// ```php
/// try { @Dom\XMLDocument::createFromFile("/nonexistent/\xff.xml"); } catch (Exception $e) { echo bin2hex($e->getMessage()), "\n"; }
/// ```
/// ```expect exit=0
/// 43616e6e6f74206f70656e2066696c6520272f6e6f6e6578697374656e742fff2e786d6c27
/// ```
pub(crate) fn xml_load(
    req: &mut Request,
    given: (PhpString, i64, Option<PhpString>),
    from_file: bool,
) -> Result<Owner<PhpObject>, Thrown> {
    let LoadArgs {
        source,
        options,
        override_encoding,
    } = xml_load_args(req, given, from_file)?;
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
    Errors::settle(req, raised, thrown)?;
    let made = match parsed {
        Parsed::Document(Some(made)) => made,
        Parsed::Malformed => {
            return Err(crate::dom_throw_message(
                req,
                Code::Syntax,
                "XML fragment is not well-formed",
            ));
        }
        Parsed::Nothing | Parsed::Document(None) => {
            if from_file {
                // `zend_throw_exception_ex`'s `%s`: the bytes as given.
                let message = [&b"Cannot open file '"[..], &source, b"'"].concat();
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(req, ce.exception, &message));
            }
            return Err(dom_throw(req, Code::InvalidState));
        }
    };
    let doc = made.node();
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
    let private = Private::new(req)?;
    let object = document_object(req, "Dom\\XMLDocument", made, private)?;
    if let Err(thrown) = convert_to_modern(req, *object, doc) {
        object.give_up(req);
        return Err(thrown);
    }
    Ok(object)
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
pub(crate) fn convert_to_modern(
    req: &mut Request,
    object: PhpObject,
    doc: f::Node,
) -> Result<(), Thrown> {
    crate::private::with_mapper(req, object, |mapper, req| {
        let mut at = doc.children();
        while let Some(node) = at {
            if node.kind() == php_ext_libxml::ffi::Kind::Element {
                mapper.mark_attribute_list(req, node)?;
            }
            at = crate::next_in_tree_order_from(node, None);
        }
        Ok(())
    })
    .map(drop)
}

/// `Dom\import_simplexml`'s answer, `Dom\Attr|Dom\Element`.
type Imported = Raw<Union<Instance<Attr>, Instance<crate::element::Element>>>;

/// `Dom\import_simplexml(object $node): Dom\Attr|Dom\Element`: the
/// element or attribute an object exports, as the modern node, its
/// document converted to the modern classes the first time.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<r a="1"/>');
/// var_dump(Dom\import_simplexml($d->documentElement) === $d->documentElement);
/// $s = simplexml_load_string('<r xmlns="urn:a">t</r>');
/// $e = Dom\import_simplexml($s);
/// var_dump(get_class($e), $e->attributes->length, Dom\import_simplexml($s) === $e);
/// try { Dom\import_simplexml(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { dom_import_simplexml($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(11) "Dom\Element"
/// int(1)
/// bool(true)
/// Dom\import_simplexml(): Argument #1 ($node) is not a valid node type
/// dom_import_simplexml(): Argument #1 ($node) must not be already imported as a Dom\Node
/// ```
#[php_function(namespace(Dom))]
pub(crate) fn import_simplexml(
    req: &mut Request,
    node: PhpObject,
) -> Result<Owner<Imported>, Thrown> {
    let found = crate::import_simplexml_common(req, node, ClassType::Modern)?;
    returned(found)
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
    let key = PhpString::new(req, name)?;
    let value = Owner::take(req, *value);
    Std.write_property(req, object, key, value)
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
pub(crate) fn clone_element(
    req: &mut Request,
    object: PhpObject,
) -> Result<Owner<PhpObject>, Thrown> {
    let clone = crate::clone_object(req, object)?;
    let forgot = forget(req, *clone, b"classList").and_then(|()| forget(req, *clone, b"children"));
    if let Err(thrown) = forgot {
        clone.give_up(req);
        return Err(thrown);
    }
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
pub(crate) fn clone_parent(
    req: &mut Request,
    object: PhpObject,
) -> Result<Owner<PhpObject>, Thrown> {
    let clone = crate::clone_object(req, object)?;
    if let Err(thrown) = forget(req, *clone, b"children") {
        clone.give_up(req);
        return Err(thrown);
    }
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

/// `class Dom\Node`, `@strict-properties`: the standard's node, whose
/// fourteen properties are `@virtual` and served by
/// [`crate::object::NODE_HANDLERS`] over the one payload, [`Dom`]. It
/// shares `DOMNode`'s bodies where php's stub says
/// `@implementation-alias`, and its own where the standard's checks are
/// not the legacy ones.
#[php_class(
    namespace(Dom),
    dynamic_properties = forbidden,
    size = 80, payload = Dom,
    handlers = crate::object::NODE_HANDLERS
)]
pub(crate) struct Node;

#[php_class]
impl Node {
    const DOCUMENT_POSITION_DISCONNECTED: i64 = 0x01;
    const DOCUMENT_POSITION_PRECEDING: i64 = 0x02;
    const DOCUMENT_POSITION_FOLLOWING: i64 = 0x04;
    const DOCUMENT_POSITION_CONTAINS: i64 = 0x08;
    const DOCUMENT_POSITION_CONTAINED_BY: i64 = 0x10;
    const DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: i64 = 0x20;
    #[php(property, virtual)]
    const nodeType: i64;
    #[php(property, virtual)]
    const nodeName: PhpString;
    #[php(property, virtual)]
    const baseURI: PhpString;
    #[php(property, virtual)]
    const isConnected: bool;
    #[php(property, virtual)]
    const ownerDocument: Option<Instance<crate::document::Document>>;
    #[php(property, virtual)]
    const parentNode: Option<Instance<Node>>;
    #[php(property, virtual)]
    const parentElement: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const childNodes: Instance<crate::map::NodeList>;
    #[php(property, virtual)]
    const firstChild: Option<Instance<Node>>;
    #[php(property, virtual)]
    const lastChild: Option<Instance<Node>>;
    #[php(property, virtual)]
    const previousSibling: Option<Instance<Node>>;
    #[php(property, virtual)]
    const nextSibling: Option<Instance<Node>>;
    #[php(property, virtual)]
    const nodeValue: Option<PhpString>;
    #[php(property, virtual)]
    const textContent: Option<PhpString>;

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
    #[php(private, final, arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let class = crate::class_name(req, this.object().class());
        let ce = Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            &format!("Cannot directly construct {class}, use document methods instead"),
        ))
    }
    #[php(implementation = DOMNode::getRootNode)]
    fn getRootNode(this: This<Self>, #[php(default = [])] options: PhpArray) -> Instance<Node>;
    #[php(implementation = DOMNode::hasChildNodes)]
    fn hasChildNodes(this: This<Self>) -> bool;
    #[php(implementation = DOMNode::normalize)]
    fn normalize(this: This<Self>);
    #[php(implementation = DOMNode::cloneNode)]
    fn cloneNode(this: This<Self>, #[php(default = false)] deep: bool) -> Instance<Node>;

    /// `Dom\Node::isEqualNode(?Dom\Node $otherNode): bool`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a xmlns:p="urn:p" x="1">t</a><a x="1">t</a><a x="1" xmlns:p="urn:p">t</a></r>');
    /// $r = $d->documentElement;
    /// $a = $r->childNodes[0];
    /// $b = $r->childNodes[1];
    /// $c = $r->childNodes[2];
    /// $copy = $d->cloneNode(true);
    /// var_dump($a->isEqualNode($b), $a->isEqualNode($c), $a->isEqualNode(null), $d->isEqualNode($copy));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// ```
    fn isEqualNode(
        req: &mut Request,
        this: This<Self>,
        otherNode: Option<Instance<Node>>,
    ) -> Result<bool, Thrown> {
        crate::node::is_equal_node_of(req, this.object(), otherNode.map(|o| o.object()), true)
    }

    /// `Dom\Node::isSameNode(?Dom\Node $otherNode): bool`, `null` never
    /// the same.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a/></r>');
    /// $r = $d->documentElement;
    /// var_dump($r->isSameNode($d->firstChild), $r->isSameNode($r->firstChild), $r->isSameNode(null));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// ```
    fn isSameNode(
        req: &mut Request,
        this: This<Self>,
        otherNode: Option<Instance<Node>>,
    ) -> Result<bool, Thrown> {
        crate::node::is_same_node(req, this.object(), otherNode.map(|o| o.object()))
    }

    #[php(implementation = DOMNode::compareDocumentPosition)]
    fn compareDocumentPosition(this: This<Self>, other: Instance<Node>) -> i64;

    /// `Dom\Node::contains(?Dom\Node $other): bool`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r><a/></r>');
    /// $a = $d->documentElement->firstChild;
    /// var_dump($d->contains($a), $a->contains($d), $a->contains($a), $d->contains(null));
    /// try { $d->contains(new DOMDocument); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// Dom\Node::contains(): Argument #1 ($other) must be of type ?Dom\Node, DOMDocument given
    /// ```
    fn contains(
        req: &mut Request,
        this: This<Self>,
        other: Option<Instance<Node>>,
    ) -> Result<bool, Thrown> {
        let Some(other) = other else {
            return Ok(false);
        };
        let other = fetch(req, other.object())?;
        let node = fetch(req, this.object())?;
        Ok(crate::node::contains_node(node, other))
    }

    /// `Dom\Node::lookupPrefix(?string $namespace): ?string`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p" xmlns:q="urn:q"><q:c a="1"/></r>');
    /// $c = $d->documentElement->firstChild;
    /// $a = $c->attributes[0];
    /// var_dump($c->lookupPrefix('urn:p'), $c->lookupPrefix('urn:q'), $a->lookupPrefix('urn:p'), $d->lookupPrefix(null), $d->lookupPrefix('urn:x'));
    /// ```
    /// ```expect
    /// string(1) "p"
    /// string(1) "q"
    /// string(1) "p"
    /// NULL
    /// NULL
    /// ```
    fn lookupPrefix(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::node::lookup_prefix(req, this.object(), namespace, true)
    }

    #[php(implementation = DOMNode::lookupNamespaceURI)]
    fn lookupNamespaceURI(this: This<Self>, prefix: Option<PhpString>) -> Option<PhpString>;

    /// `Dom\Node::isDefaultNamespace(?string $namespace): bool`: the default
    /// namespace the standard locates, null and the empty string alike.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns="urn:d"><c xmlns=""/></r>');
    /// $c = $d->documentElement->firstChild;
    /// var_dump($d->isDefaultNamespace('urn:d'), $c->isDefaultNamespace(null), $c->isDefaultNamespace(''), $c->isDefaultNamespace('urn:d'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    fn isDefaultNamespace(
        req: &mut Request,
        this: This<Self>,
        namespace: Option<PhpString>,
    ) -> Result<bool, Thrown> {
        let uri = namespace
            .map(|n| n.as_bytes().to_vec())
            .filter(|u| !u.is_empty());
        let node = fetch(req, this.object())?;
        let located = crate::node::locate_namespace(node, None);
        let uri = uri.as_deref().map(php_ext::coerce::c_str);
        Ok(located.as_deref() == uri)
    }

    /// `Dom\Node::insertBefore(Dom\Node $node, ?Dom\Node $child): Dom\Node`:
    /// the standard's pre-insert.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r><r><b/></r>');
    /// $r = $d->documentElement;
    /// $b = $r->firstChild;
    /// $r->insertBefore($d->createComment('a'), $b);
    /// $r->insertBefore($d->createTextNode('c'), null);
    /// $a = $r->firstChild;
    /// $r->insertBefore($a, null);
    /// foreach ($r->childNodes as $n) echo $n->nodeName, "\n";
    /// $x = $d->createComment('x');
    /// $dt = $d->doctype;
    /// foreach ([fn() => $d->insertBefore($x, $b), fn() => $d->insertBefore($x, $dt), fn() => $d->insertBefore($dt, $r), fn() => $d->insertBefore($r->cloneNode(), $dt)] as $f) {
    ///     try { $f(); echo "ok\n"; } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// b
    /// #text
    /// #comment
    /// Not Found Error
    /// ok
    /// Cannot have more than one document type
    /// Cannot have more than one element child in a document
    /// ```
    fn insertBefore(
        req: &mut Request,
        this: This<Self>,
        node: Instance<Node>,
        child: Option<Instance<Node>>,
    ) -> Result<Owner<Raw<Instance<Node>>>, Thrown> {
        let this = this.object();
        let parent = fetch(req, this)?;
        let node = fetch(req, node.object())?;
        if is_parent_invalid(parent) {
            return Err(dom_throw(req, Code::HierarchyRequest));
        }
        let point = match child {
            Some(r) => Some(fetch(req, r.object())?),
            None => None,
        };
        invalidate(req, this);
        crate::parent::pre_insert(req, this, node, parent, point)?;
        let inserted = object_for(req, node, Some(this))?;
        returned(inserted)
    }

    /// `Dom\Node::appendChild(Dom\Node $node): Dom\Node`: the standard's
    /// append, which checks what a document may hold and merges no text.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r>a</r>');
    /// $r = $d->documentElement;
    /// $t = $r->appendChild($d->createTextNode('b'));
    /// $first = $r->firstChild;
    /// var_dump($r->childNodes->length, $t->data, $r->appendChild($first) === $first, $r->lastChild->data);
    /// $text = $d->createTextNode('t');
    /// $copy = $r->cloneNode();
    /// $comment = $d->createComment('c');
    /// foreach ([fn() => $d->appendChild($text), fn() => $d->appendChild($copy), fn() => $r->appendChild($r), fn() => $t->appendChild($comment)] as $f) {
    ///     try { $f(); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// int(2)
    /// string(1) "b"
    /// bool(true)
    /// string(1) "a"
    /// Cannot insert text as a child of a document
    /// Cannot have more than one element child in a document
    /// Hierarchy Request Error
    /// Hierarchy Request Error
    /// ```
    fn appendChild(
        req: &mut Request,
        this: This<Self>,
        node: Instance<Node>,
    ) -> Result<Owner<Raw<Instance<Node>>>, Thrown> {
        let this = this.object();
        let parent = fetch(req, this)?;
        let node = fetch(req, node.object())?;
        if is_parent_invalid(parent) {
            return Err(dom_throw(req, Code::HierarchyRequest));
        }
        invalidate(req, this);
        crate::parent::pre_insert(req, this, node, parent, None)?;
        let appended = object_for(req, node, Some(this))?;
        returned(appended)
    }

    /// `Dom\Node::replaceChild(Dom\Node $node, Dom\Node $child): Dom\Node`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r><r><a/><b/></r>');
    /// $r = $d->documentElement;
    /// $a = $r->firstChild;
    /// $b = $r->lastChild;
    /// $old = $r->replaceChild($b, $a);
    /// echo $old->nodeName, ' ', $r->childNodes->length, ' ', $r->firstChild->nodeName, "\n";
    /// $t = $d->createTextNode('t');
    /// $c = $b->cloneNode();
    /// $dt = $d->doctype;
    /// foreach ([fn() => $d->replaceChild($t, $r), fn() => $d->replaceChild($c, $dt), fn() => $r->replaceChild($r, $b)] as $f) {
    ///     try { $f(); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// a 1 b
    /// Cannot insert text as a child of a document
    /// Cannot have more than one element child in a document
    /// Hierarchy Request Error
    /// ```
    fn replaceChild(
        req: &mut Request,
        this: This<Self>,
        node: Instance<Node>,
        child: Instance<Node>,
    ) -> Result<Owner<Raw<Instance<Node>>>, Thrown> {
        let old =
            crate::node::replace_child(req, this.object(), node.object(), child.object(), true)?;
        returned(old)
    }

    #[php(implementation = DOMNode::removeChild)]
    fn removeChild(this: This<Self>, child: Instance<Node>) -> Instance<Node>;
    #[php(implementation = DOMNode::getLineNo)]
    fn getLineNo(this: This<Self>) -> i64;

    /// `Dom\Node::getNodePath(): string`.
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r xmlns:p="urn:p"><a/><p:a><b x="1"/></p:a></r>');
    /// $b = $d->documentElement->lastChild->firstChild;
    /// echo $b->getNodePath(), ' ', $b->attributes[0]->getNodePath(), ' ', $d->getNodePath(), "\n";
    /// ```
    /// ```expect
    /// /r/p:a/b /r/p:a/b/@x /
    /// ```
    fn getNodePath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        match fetch(req, this.object())?.path() {
            Some(p) => PhpString::new(req, &p),
            None => Err(dom_throw(req, Code::InvalidState)),
        }
    }

    #[php(implementation = DOMNode::C14N)]
    fn C14N(
        this: This<Self>,
        #[php(default = false)] exclusive: bool,
        #[php(default = false)] withComments: bool,
        #[php(default = null)] xpath: Option<PhpArray>,
        #[php(default = null)] nsPrefixes: Option<PhpArray>,
    ) -> OrFalse<PhpString>;
    #[php(implementation = DOMNode::C14NFile)]
    fn C14NFile(
        this: This<Self>,
        uri: PhpString,
        #[php(default = false)] exclusive: bool,
        #[php(default = false)] withComments: bool,
        #[php(default = null)] xpath: Option<PhpArray>,
        #[php(default = null)] nsPrefixes: Option<PhpArray>,
    ) -> OrFalse<i64>;
    #[php(implementation = DOMNode::__sleep)]
    fn __sleep(this: This<Self>) -> PhpArray;
    #[php(implementation = DOMNode::__wakeup)]
    fn __wakeup(this: This<Self>);
}

/// `readonly final class Dom\NamespaceInfo`, `@not-serializable` and
/// `@strict-properties`: what `getInScopeNamespaces` answers, three
/// readonly properties in slots, and `Dom\Node`'s refusing constructor.
#[php_class(
    namespace(Dom),
    final,
    readonly,
    not_serializable,
    dynamic_properties = forbidden
)]
pub(crate) struct NamespaceInfo;

#[php_class]
impl NamespaceInfo {
    #[php(property, readonly)]
    const prefix: Option<PhpString>;
    #[php(property, readonly)]
    const namespaceURI: Option<PhpString>;
    #[php(property, readonly)]
    const element: Instance<crate::element::Element>;
    #[php(private, implementation = Node::__construct)]
    fn __construct(this: This<Self>);
}

/// `class Dom\Attr extends Dom\Node`: an attribute, its seven properties
/// `@virtual`; `isId` is `DOMAttr`'s and `rename` `Dom\Element`'s
/// (`@implementation-alias`).
#[php_class(namespace(Dom), extends(Node))]
pub(crate) struct Attr;

#[php_class]
impl Attr {
    #[php(property, virtual)]
    const namespaceURI: Option<PhpString>;
    #[php(property, virtual)]
    const prefix: Option<PhpString>;
    #[php(property, virtual)]
    const localName: PhpString;
    #[php(property, virtual)]
    const name: PhpString;
    #[php(property, virtual)]
    const value: PhpString;
    #[php(property, virtual)]
    const ownerElement: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const specified: bool;
    #[php(implementation = crate::classes::DOMAttr::isId)]
    fn isId(this: This<Self>) -> bool;
    #[php(implementation = crate::element::Element::rename)]
    fn rename(this: This<Self>, namespaceURI: Option<PhpString>, qualifiedName: PhpString);
}

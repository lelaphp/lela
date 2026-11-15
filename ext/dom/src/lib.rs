//! The `dom` extension: `php-src/ext/dom`'s legacy classes as one crate
//! under `ext/` (ADR-0004), over libxml2 through `ext/libxml`'s surface
//! (design §4.10), which is `php_libxml.h`'s. Its state on the value
//! (ADR-0005). The phpt subset is `php-src/ext/dom/tests`, through `xtask
//! phpt --subset`.
//!
//! Every node class's object carries one payload, [`object::Dom`], php's
//! `dom_object`: the libxml node object (`php_libxml_node_object`) that
//! names the node through its proxy and counts its document, and what a
//! list, a map, an evaluator or a namespace node keeps beside it. A node
//! answers as the same object while one stands for it, and a document
//! lives while any object of its tree does. The properties are virtual,
//! read and written through the handler tables of [`props`] in
//! `PHP_MINIT_FUNCTION(dom)`'s order. The `Dom\` namespace, HTML5 and
//! lexbor are not here.
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

mod classes;
mod css;
mod document;
mod element;
mod html5_parser;
mod html5_serializer;
mod html_document;
mod inner_html;
mod map;
mod modern;
mod node;
mod object;
mod parent;
mod private;
mod props;
mod throwables;
mod token_list;
mod xml_serializer;
mod xpath;

use php_ext::abi::{
    DebugInfo, Dependency, DependencyKind, Extension, ExtensionBuilder, FromArgument, Instance,
    ObjectHandlers, OrFalse, Passed, PropertyPlace, PropertyRead, PropertyTest, Raw, Std, Union,
    Untyped,
};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{to_string, truthy};
use php_ext::raising::{
    Level, Thrown, argument_error, deprecated, fatal, new_throwable, throw_error, throw_value,
    type_error,
};
use php_ext::request::{MAXPATHLEN, Request};
use php_ext::values::walk;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{
    Borrowed, ClassType, DocOf, DocProps, Exported, Kind, Name, Node, NodeObject, Ns, Switches,
    XML_ATTRIBUTE_ID,
};

use document::{XML_NS, XMLNS_NS};
use map::{Map, MapKind};
use object::{Code, Dom, Extra, State};
use props::Prop;
pub use xpath::{CallbacksAt, Proxy, XPathCallbacks};

/// Core's interfaces the lists implement and hand out, named for
/// `implements` and the types that hold them.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

#[php_class(extern)]
pub(crate) struct Countable;

#[php_class(extern)]
pub(crate) struct Iterator;

/// A property read's answer: `None` is php's `FAILURE`, a row a dumper
/// leaves out.
type Read = Result<Option<Owner<Value>>, Thrown>;

/// Core's `Exception`, named for `extends`.
#[php_class(extern)]
pub(crate) struct Exception;

/// `final class DOMException extends Exception`, its `$code` left untyped
/// for BC, as php's stub says; `Dom\DOMException` is its `@alias`, which
/// `register_class_DOMException` registers right after the class, so it
/// stands there among the classes every request starts with.
///
/// ```php
/// $e = new DOMException('m', 3);
/// var_dump($e->code, $e->getCode(), (new ReflectionProperty('DOMException', 'code'))->hasType());
/// var_dump(new Dom\DOMException('m') instanceof DOMException, class_exists('Dom\DOMException'));
/// $c = get_declared_classes();
/// $i = array_search("dom\\domexception", $c);
/// var_dump($c[$i - 1], $c[$i + 1] ?? null);
/// ```
/// ```expect
/// int(3)
/// int(3)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(12) "DOMException"
/// string(17) "DOMImplementation"
/// ```
#[php_class(final, extends(Exception), alias = "Dom\\DOMException")]
pub(crate) struct DOMException;

#[php_class]
impl DOMException {
    #[php(property)]
    const code: Untyped<Value> = 0;
}

/// The extension value (design §7.1).
///
/// ```php
/// var_dump(extension_loaded('dom'), XML_ELEMENT_NODE, DOM_NAMESPACE_ERR);
/// print_r((new ReflectionExtension('dom'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// int(1)
/// int(14)
/// Array
/// (
///     [libxml] => Required
///     [lexbor] => Required
///     [domxml] => Conflicts
/// )
/// ```
///
/// Every class, function and constant is declared by the attributes, in
/// `PHP_MINIT_FUNCTION(dom)`'s order:
///
/// ```php
/// var_dump(DOMNode::DOCUMENT_POSITION_CONTAINED_BY, (new ReflectionMethod("DOMNode", "getLineNo"))->hasTentativeReturnType());
/// echo implode(",", (new ReflectionClass("DOMElement"))->getInterfaceNames()), "\n";
/// echo (new ReflectionMethod("DOMDocument", "__construct"))->getParameters()[0], "\n";
/// echo (new ReflectionFunction("dom_import_simplexml"))->getReturnType(), "\n";
/// $c = (new ReflectionExtension("dom"))->getClassNames();
/// echo implode(",", array_slice($c, 0, 2)), " ", implode(",", array_slice($c, -3)), "\n";
/// ```
/// ```expect
/// int(16)
/// bool(true)
/// DOMParentNode,DOMChildNode
/// Parameter #0 [ <optional> string $version = "1.0" ]
/// DOMAttr|DOMElement
/// Dom\AdjacentPosition,DOMException DOMXPath,Dom\XPath,Dom\TokenList
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[
            dom_import_simplexml::DECLARED,
            modern::import_simplexml::DECLARED,
        ])
        .constants(constants::DECLARED)
        .constants(modern_constants::DECLARED)
        .register(
            php_ext_libxml::register_export(b"DOMNode", export_node).with_node_object(libxml_half),
        )
        .register(
            php_ext_libxml::register_export(b"Dom\\Node", export_node)
                .with_node_object(libxml_half),
        )
        .dependencies(vec![
            dependency("libxml", DependencyKind::Required),
            dependency("lexbor", DependencyKind::Required),
            dependency("domxml", DependencyKind::Conflicts),
        ])
        .engine_state(throwables::Throwables::start)
        .class::<element::AdjacentPosition>()
        .class::<DOMException>()
        .class::<parent::DOMParentNode>()
        .class::<parent::ParentNode>()
        .class::<parent::DOMChildNode>()
        .class::<parent::ChildNode>()
        .class::<classes::DOMImplementation>()
        .class::<classes::Implementation>()
        .class::<node::DOMNode>()
        .class::<modern::Node>()
        .class::<node::DOMNameSpaceNode>()
        .class::<modern::NamespaceInfo>()
        .class::<classes::DOMDocumentFragment>()
        .class::<classes::DocumentFragment>()
        .class::<document::Document>()
        .class::<document::DOMDocument>()
        .class::<html_document::HTMLDocument>()
        .class::<document::XMLDocument>()
        .class::<map::DOMNodeList>()
        .class::<map::NodeList>()
        .class::<map::DOMNamedNodeMap>()
        .class::<map::NamedNodeMap>()
        .class::<map::DtdNamedNodeMap>()
        .class::<map::HTMLCollection>()
        .class::<classes::DOMCharacterData>()
        .class::<classes::CharacterData>()
        .class::<classes::DOMAttr>()
        .class::<modern::Attr>()
        .class::<element::DOMElement>()
        .class::<element::Element>()
        .class::<element::HTMLElement>()
        .class::<classes::DOMText>()
        .class::<classes::Text>()
        .class::<classes::DOMComment>()
        .class::<classes::Comment>()
        .class::<classes::DOMCdataSection>()
        .class::<classes::CDATASection>()
        .class::<classes::DOMDocumentType>()
        .class::<classes::DocumentType>()
        .class::<classes::DOMNotation>()
        .class::<classes::Notation>()
        .class::<classes::DOMEntity>()
        .class::<classes::Entity>()
        .class::<classes::DOMEntityReference>()
        .class::<classes::EntityReference>()
        .class::<classes::DOMProcessingInstruction>()
        .class::<classes::ProcessingInstruction>()
        .class::<xpath::DOMXPath>()
        .class::<xpath::XPath>()
        .class::<token_list::TokenList>()
        .request_state::<object::State>()
        .info(dom_info)
        .version("20031129")
        .build("dom")
}

fn dependency(name: &str, kind: DependencyKind) -> Dependency {
    Dependency {
        name: String::from(name),
        kind,
        rel: None,
        version: None,
    }
}

/// `register_php_dom_symbols`: the node types, the attribute types and the
/// error codes, `DOM_PHP_ERR` deprecated since 8.4; each code is its
/// [`Code`].
#[php_const]
pub(crate) mod constants {
    use super::Code;
    pub(crate) const XML_ELEMENT_NODE: i64 = 1;
    pub(crate) const XML_ATTRIBUTE_NODE: i64 = 2;
    pub(crate) const XML_TEXT_NODE: i64 = 3;
    pub(crate) const XML_CDATA_SECTION_NODE: i64 = 4;
    pub(crate) const XML_ENTITY_REF_NODE: i64 = 5;
    pub(crate) const XML_ENTITY_NODE: i64 = 6;
    pub(crate) const XML_PI_NODE: i64 = 7;
    pub(crate) const XML_COMMENT_NODE: i64 = 8;
    pub(crate) const XML_DOCUMENT_NODE: i64 = 9;
    pub(crate) const XML_DOCUMENT_TYPE_NODE: i64 = 10;
    pub(crate) const XML_DOCUMENT_FRAG_NODE: i64 = 11;
    pub(crate) const XML_NOTATION_NODE: i64 = 12;
    pub(crate) const XML_HTML_DOCUMENT_NODE: i64 = 13;
    pub(crate) const XML_DTD_NODE: i64 = 14;
    pub(crate) const XML_ELEMENT_DECL_NODE: i64 = 15;
    pub(crate) const XML_ATTRIBUTE_DECL_NODE: i64 = 16;
    pub(crate) const XML_ENTITY_DECL_NODE: i64 = 17;
    pub(crate) const XML_NAMESPACE_DECL_NODE: i64 = 18;
    pub(crate) const XML_LOCAL_NAMESPACE: i64 = 18;
    pub(crate) const XML_ATTRIBUTE_CDATA: i64 = 1;
    pub(crate) const XML_ATTRIBUTE_ID: i64 = 2;
    pub(crate) const XML_ATTRIBUTE_IDREF: i64 = 3;
    pub(crate) const XML_ATTRIBUTE_IDREFS: i64 = 4;
    pub(crate) const XML_ATTRIBUTE_ENTITY: i64 = 6;
    pub(crate) const XML_ATTRIBUTE_NMTOKEN: i64 = 7;
    pub(crate) const XML_ATTRIBUTE_NMTOKENS: i64 = 8;
    pub(crate) const XML_ATTRIBUTE_ENUMERATION: i64 = 9;
    pub(crate) const XML_ATTRIBUTE_NOTATION: i64 = 10;
    #[php(deprecated(since = "8.4", message = "as it is no longer used"))]
    pub(crate) const DOM_PHP_ERR: i64 = 0;
    pub(crate) const DOM_INDEX_SIZE_ERR: i64 = Code::IndexSize as i64;
    pub(crate) const DOMSTRING_SIZE_ERR: i64 = Code::StringSize as i64;
    pub(crate) const DOM_HIERARCHY_REQUEST_ERR: i64 = Code::HierarchyRequest as i64;
    pub(crate) const DOM_WRONG_DOCUMENT_ERR: i64 = Code::WrongDocument as i64;
    pub(crate) const DOM_INVALID_CHARACTER_ERR: i64 = Code::InvalidCharacter as i64;
    pub(crate) const DOM_NO_DATA_ALLOWED_ERR: i64 = Code::NoDataAllowed as i64;
    pub(crate) const DOM_NO_MODIFICATION_ALLOWED_ERR: i64 = Code::NoModificationAllowed as i64;
    pub(crate) const DOM_NOT_FOUND_ERR: i64 = Code::NotFound as i64;
    pub(crate) const DOM_NOT_SUPPORTED_ERR: i64 = Code::NotSupported as i64;
    pub(crate) const DOM_INUSE_ATTRIBUTE_ERR: i64 = Code::InuseAttribute as i64;
    pub(crate) const DOM_INVALID_STATE_ERR: i64 = Code::InvalidState as i64;
    pub(crate) const DOM_SYNTAX_ERR: i64 = Code::Syntax as i64;
    pub(crate) const DOM_INVALID_MODIFICATION_ERR: i64 = Code::InvalidModification as i64;
    pub(crate) const DOM_NAMESPACE_ERR: i64 = Code::Namespace as i64;
    pub(crate) const DOM_INVALID_ACCESS_ERR: i64 = Code::InvalidAccess as i64;
    pub(crate) const DOM_VALIDATION_ERR: i64 = Code::Validation as i64;
}

/// The `Dom\` codes, which have no `INVALID_ACCESS_ERR`, and
/// `Dom\HTML_NO_DEFAULT_NS`.
#[php_const(namespace(Dom))]
pub(crate) mod modern_constants {
    use super::Code;
    pub(crate) const INDEX_SIZE_ERR: i64 = Code::IndexSize as i64;
    pub(crate) const STRING_SIZE_ERR: i64 = Code::StringSize as i64;
    pub(crate) const HIERARCHY_REQUEST_ERR: i64 = Code::HierarchyRequest as i64;
    pub(crate) const WRONG_DOCUMENT_ERR: i64 = Code::WrongDocument as i64;
    pub(crate) const INVALID_CHARACTER_ERR: i64 = Code::InvalidCharacter as i64;
    pub(crate) const NO_DATA_ALLOWED_ERR: i64 = Code::NoDataAllowed as i64;
    pub(crate) const NO_MODIFICATION_ALLOWED_ERR: i64 = Code::NoModificationAllowed as i64;
    pub(crate) const NOT_FOUND_ERR: i64 = Code::NotFound as i64;
    pub(crate) const NOT_SUPPORTED_ERR: i64 = Code::NotSupported as i64;
    pub(crate) const INUSE_ATTRIBUTE_ERR: i64 = Code::InuseAttribute as i64;
    pub(crate) const INVALID_STATE_ERR: i64 = Code::InvalidState as i64;
    pub(crate) const SYNTAX_ERR: i64 = Code::Syntax as i64;
    pub(crate) const INVALID_MODIFICATION_ERR: i64 = Code::InvalidModification as i64;
    pub(crate) const NAMESPACE_ERR: i64 = Code::Namespace as i64;
    pub(crate) const VALIDATION_ERR: i64 = Code::Validation as i64;
    /// `DOM_HTML_NO_DEFAULT_NS`, the option that keeps the HTML5 parser's
    /// elements out of the HTML namespace.
    pub(crate) const HTML_NO_DEFAULT_NS: i64 = 1 << 31;
}

/// `PHP_MINFO_FUNCTION(dom)`.
///
/// ```php
/// (new ReflectionExtension('dom'))->info();
/// ```
/// ```expect
///
/// dom
///
/// DOM/XML => enabled
/// DOM/XML API Version => 20031129
/// libxml Version => 2.9.13
/// HTML Support => enabled
/// XPath Support => enabled
/// XPointer Support => enabled
/// Schema Support => enabled
/// RelaxNG Support => enabled
/// ```
fn dom_info(req: &mut Request) {
    req.extensions_mut().info_table(&[
        &["DOM/XML", "enabled"],
        &["DOM/XML API Version", "20031129"],
        &["libxml Version", "2.9.13"],
        &["HTML Support", "enabled"],
        &["XPath Support", "enabled"],
        &["XPointer Support", "enabled"],
        &["Schema Support", "enabled"],
        &["RelaxNG Support", "enabled"],
    ]);
}

/// `dom_import_simplexml`'s answer, `DOMAttr|DOMElement`.
type Imported = Raw<Union<Instance<classes::DOMAttr>, Instance<element::DOMElement>>>;

/// `dom_import_simplexml(object $node): DOMAttr|DOMElement`: the element
/// or attribute any XML extension's object stands for, through libxml's
/// table of exports (`php_libxml_import_node`), as the legacy node sharing that
/// object's document.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// var_dump(dom_import_simplexml($d->documentElement) === $d->documentElement);
/// $s = simplexml_load_string('<r a="1"><c/></r>');
/// $e = dom_import_simplexml($s->c);
/// var_dump(get_class($e), $e->ownerDocument->saveXML($e->parentNode), dom_import_simplexml($s->c) === $e);
/// var_dump(dom_import_simplexml($s->attributes())->value);
/// try { dom_import_simplexml(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { Dom\import_simplexml($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(10) "DOMElement"
/// string(17) "<r a="1"><c/></r>"
/// bool(true)
/// string(1) "1"
/// dom_import_simplexml(): Argument #1 ($node) is not a valid node type
/// Dom\import_simplexml(): Argument #1 ($node) must not be already imported as a DOMNode
/// ```
#[php_function]
fn dom_import_simplexml(req: &mut Request, node: PhpObject) -> Result<Owner<Imported>, Thrown> {
    let found = import_simplexml_common(req, node, ClassType::Legacy)?;
    returned(found)
}

/// `dom_import_simplexml_common`: the node an object exports, locked to
/// `new_class` on its document and answered as that family's node.
fn import_simplexml_common(
    req: &mut Request,
    object: PhpObject,
    new_class: ClassType,
) -> Result<Owner<Value>, Thrown> {
    let ce = throwables::Throwables::of(req)?;
    let exported = php_ext_libxml::import_node(req, object)
        .filter(Exported::has_document)
        .filter(|e| matches!(e.node().kind(), Kind::Element | Kind::Attribute));
    let Some(exported) = exported else {
        return Err(argument_error(
            req,
            ce.type_error,
            1,
            "is not a valid node type",
        ));
    };
    let old = exported.class_type();
    if old != ClassType::Unset && old != new_class {
        let family = if new_class == ClassType::Modern {
            "a DOMNode"
        } else {
            "a Dom\\Node"
        };
        let tail = format!("must not be already imported as {family}");
        return Err(argument_error(req, ce.type_error, 1, &tail));
    }
    exported.set_class_type(new_class);
    let node = exported.node();
    if let Some(existing) = node.object() {
        return Ok(Owner::take(req, existing.as_value()));
    }
    let Some(name) = class_for(node, new_class == ClassType::Modern) else {
        let n = node.kind().number();
        return Err(throw_error(
            req,
            ce.error,
            &format!("Unsupported node type: {n}"),
        ));
    };
    let base = class(req, name)?;
    let class = exported.mapped(base);
    let made = new_object(req, class)?;
    let Some(of) = DocOf::exported(&exported) else {
        made.give_up(req);
        return Err(DocOf::uncounted(req));
    };
    if let Some(mut obj) = libxml_half(req, *made) {
        obj.attach(req, of);
    }
    if old != ClassType::Modern
        && new_class == ClassType::Modern
        && let Some(doc) = node.doc()
    {
        let private = private::Private::new(req)?;
        match libxml_half(req, *made) {
            Some(mut obj) => obj.set_private_data(req, Box::new(private)),
            None => private.free(req),
        }
        if let Err(thrown) = modern::convert_to_modern(req, *made, doc) {
            made.give_up(req);
            return Err(thrown);
        }
    }
    Ok(made.into_value())
}

/// `php_dom_export_node`: the node a dom object stands for, and its
/// document, what `php_libxml_import_node` answers for `DOMNode` and
/// `Dom\Node`.
fn export_node(req: &mut Request, object: PhpObject) -> Option<Exported> {
    let d = dom(req, object)?;
    Some(Exported::of(d.obj.bound()?, &d.obj))
}

// ---- the helpers every class module shares

/// The payload of `object`.
fn dom(req: &Request, object: PhpObject) -> Option<&Dom> {
    object.native::<Dom>(req)
}

/// The payload of `object`, to write.
fn dom_mut(req: &mut Request, object: PhpObject) -> Option<&mut Dom> {
    object.native_mut::<Dom>(req)
}

/// `dom_object_get_node`.
fn node_of(req: &Request, object: PhpObject) -> Option<Node> {
    dom(req, object).and_then(|d| d.obj.bound())
}

/// `dom_get_doc_props_read_only(obj->document)`.
fn props_of(req: &Request, object: PhpObject) -> Switches {
    dom(req, object).map(|d| d.obj.props()).unwrap_or_default()
}

/// `dom_get_strict_error(obj->document)`.
fn strict(req: &Request, object: PhpObject) -> bool {
    props_of(req, object).strict_error
}

/// `php_libxml_invalidate_node_list_cache(obj->document)`.
fn invalidate(req: &mut Request, object: PhpObject) {
    if let Some(d) = dom_mut(req, object) {
        d.obj.invalidate();
    }
}

/// `dom_minimum_modification_nr_since_parsing`: a legacy document takes a
/// `new` and a load to reach its parsed state, a modern one only its
/// named constructor.
fn minimum_modification(obj: &php_ext_libxml::ffi::NodeObject) -> usize {
    if obj.class_type() == php_ext_libxml::ffi::ClassType::Modern {
        1
    } else {
        2
    }
}

/// `dom_is_document_cache_modified_since_parsing`: whether the tree may
/// have changed since the parse, so libxml's ID cache is not to be
/// trusted.
///
/// ```php
/// $d = \Dom\HTMLDocument::createFromString("<p id=a>b</p><p id=a>c</p>", LIBXML_NOERROR);
/// $p = $d->getElementById("a");
/// echo $p->textContent, "\n";
/// $p->remove();
/// echo $d->getElementById("a")->textContent, "\n";
/// $x = Dom\XMLDocument::createFromString("<r xmlns:x=\"urn:x\" x:a=\"1\"/>");
/// foreach ($x->documentElement->attributes as $k => $v) echo $k, "=", $v->value, "\n";
/// ```
/// ```expect
/// b
/// c
/// xmlns:x=urn:x
/// x:a=1
/// ```
fn modified_since_parsing(req: &Request, object: PhpObject) -> bool {
    dom(req, object).is_none_or(|d| {
        !d.obj.has_document() || d.obj.modification() > minimum_modification(&d.obj)
    })
}

/// `dom_mark_ids_modified(obj->document)`: the modification number past
/// the minimum a fresh parse leaves, so `getElementById` stops trusting
/// libxml's ID cache.
fn mark_ids_modified(req: &mut Request, object: PhpObject) {
    if let Some(d) = dom_mut(req, object)
        && d.obj.has_document()
    {
        let at_least = minimum_modification(&d.obj) + 1;
        if d.obj.modification() < at_least {
            d.obj.set_modification(at_least);
        }
    }
}

/// `ce->name` of a class.
pub(crate) fn class_name(req: &Request, class: ClassId) -> String {
    req.symbols()
        .class_decl(class)
        .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
        .unwrap_or_default()
}

/// The class a dom name answers, a fatal where the bundle lacks it.
fn class(req: &mut Request, name: &str) -> Result<ClassId, Thrown> {
    match lookup_class(req, name.as_bytes())? {
        Some(class) => Ok(class),
        None => Err(fatal(
            req,
            &format!("InternalError: {name} is not declared"),
        )),
    }
}

/// `instanceof_function(object's class, name)`.
fn instance_of(req: &mut Request, object: PhpObject, name: &str) -> bool {
    match lookup_class(req, name.as_bytes()) {
        Ok(Some(class)) => object.instance_of(req, class),
        _ => false,
    }
}

/// The property handlers of an object's class, `obj->prop_handler`.
fn table_of(req: &mut Request, object: PhpObject) -> Option<&'static [&'static [Prop]]> {
    let base = base_name(req, object.class())?;
    table(&base)
}

/// The rows `dom_get_debug_info_helper` walks: `table_of`'s rows, but
/// `Dom\Node`'s alone for a `Dom\Notation`, so its dump keeps the oracle's
/// rows and no id.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<!DOCTYPE r [<!NOTATION n PUBLIC "p" "s">]><r/>');
/// var_dump($d->doctype->notations->getNamedItem('n'));
/// ```
/// ```expect
/// object(Dom\Notation)#4 (13) {
///   ["nodeType"]=>
///   int(12)
///   ["nodeName"]=>
///   string(1) "n"
///   ["baseURI"]=>
///   string(11) "about:blank"
///   ["isConnected"]=>
///   bool(false)
///   ["parentNode"]=>
///   NULL
///   ["parentElement"]=>
///   NULL
///   ["childNodes"]=>
///   string(22) "(object value omitted)"
///   ["firstChild"]=>
///   NULL
///   ["lastChild"]=>
///   NULL
///   ["previousSibling"]=>
///   NULL
///   ["nextSibling"]=>
///   NULL
///   ["nodeValue"]=>
///   NULL
///   ["textContent"]=>
///   string(0) ""
/// }
/// ```
fn dump_table_of(req: &mut Request, object: PhpObject) -> Option<&'static [&'static [Prop]]> {
    let base = base_name(req, object.class())?;
    if base == b"Dom\\Notation" {
        return Some(&[crate::props::MODERN_NODE]);
    }
    table(&base)
}

/// `php_dom_throw_error_with_message`: a `DOMException` of `code` where
/// errors are strict, the message raised as a warning (or onto libxml's
/// list) where they are not; `Err` is what was thrown.
fn dom_error_message(
    req: &mut Request,
    code: Code,
    message: &str,
    strict: bool,
) -> Result<(), Thrown> {
    if strict {
        let ce = throwables::Throwables::of(req)?;
        let exception = new_throwable(req, ce.dom_exception, message)?;
        if let Some(object) = exception.as_object() {
            let code = Owner::int(code as i64);
            let set = php_ext::call::write_prop(req, &object, b"code", code);
            if let Err(thrown) = set {
                exception.give_up(req);
                return Err(thrown);
            }
        }
        let thrown = throw_value(req, &exception);
        exception.give_up(req);
        return Err(thrown);
    }
    php_ext_libxml::Errors::issue(req, Level::Warning, message);
    Ok(())
}

/// `php_dom_throw_error`.
fn dom_error(req: &mut Request, code: Code, strict: bool) -> Result<(), Thrown> {
    dom_error_message(req, code, code.message(), strict)
}

/// A `DOMException` of `code` with `message` thrown whatever strictness
/// says, `php_dom_throw_error_with_message(code, message, true)`.
fn dom_throw_message(req: &mut Request, code: Code, message: &str) -> Thrown {
    match dom_error_message(req, code, message, true) {
        Err(thrown) => thrown,
        Ok(()) => fatal(req, "InternalError: a strict DOMException was not thrown"),
    }
}

/// A `DOMException` of `code` thrown whatever strictness says.
fn dom_throw(req: &mut Request, code: Code) -> Thrown {
    match dom_error(req, code, true) {
        Err(thrown) => thrown,
        Ok(()) => fatal(req, "InternalError: a strict DOMException was not thrown"),
    }
}

/// `DOM_GET_OBJ`: the node an object stands for, php's `Error` "Couldn't
/// fetch" where it stands for none.
fn fetch(req: &mut Request, object: PhpObject) -> Result<Node, Thrown> {
    if let Some(node) = node_of(req, object) {
        return Ok(node);
    }
    let name = class_name(req, object.class());
    let ce = throwables::Throwables::of(req)?;
    Err(throw_error(
        req,
        ce.error,
        &format!("Couldn't fetch {name}"),
    ))
}

/// `DOM_GET_INTERN` of the receiver: php's "Couldn't fetch" where its
/// payload stands for nothing.
fn intern(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    let bound = dom(req, this).is_some_and(|d| d.obj.is_bound() || !matches!(d.extra, Extra::None));
    if bound {
        return Ok(this);
    }
    let name = class_name(req, this.class());
    let ce = throwables::Throwables::of(req)?;
    Err(throw_error(
        req,
        ce.error,
        &format!("Couldn't fetch {name}"),
    ))
}

/// php's `ZVAL_STRING`: a new string of `bytes`, one owner the
/// caller's, `zend_string_init` whatever its length, so never the
/// interned empty or one-byte string [`PhpString::new`] answers with.
/// What ext/dom's readers and natives hand out for libxml's bytes.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    let made = PhpString::filled(req, bytes.len(), |out| out.copy_from_slice(bytes))?;
    Ok(made.into_value())
}

/// php's `ZVAL_EMPTY_STRING`: the interned empty string, where a body
/// answers nothing found with it rather than with a string it made.
fn empty_string(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    Ok(PhpString::new(req, b"")?.into_value())
}

/// php's `smart_str_extract`: the interned empty string where nothing
/// was appended, and the buffer's own new string otherwise.
fn extracted(req: &mut Request, bytes: &[u8]) -> Result<Owner<Value>, Thrown> {
    if bytes.is_empty() {
        return empty_string(req);
    }
    string(req, bytes)
}

/// A body's value returned under its declared type `T`, as php returns
/// one: php verifies a native's return in its debug builds only.
#[allow(clippy::unnecessary_wraps)] // the tail of every native that returns it
fn returned<T: FromArgument>(value: Owner<Value>) -> Result<Owner<Raw<T>>, Thrown> {
    Ok(value.into_raw())
}

/// A body's `T|false` returned under its declared type: `false` as
/// itself, anything else under `T`.
fn or_false<T: FromArgument>(
    req: &mut Request,
    value: Owner<Value>,
) -> Result<OrFalse<Owner<Raw<T>>>, Thrown> {
    if value.as_bool() == Some(false) {
        value.give_up(req);
        return Ok(OrFalse::False);
    }
    returned(value).map(OrFalse::Value)
}

/// The bytes of an optional string argument, none where the call gave
/// none, as `|s` leaves a C string `NULL`.
fn passed(given: Passed<PhpString>) -> Option<Vec<u8>> {
    match given {
        Passed::Given(s) => Some(s.as_bytes().to_vec()),
        Passed::Omitted => None,
    }
}

/// A string value, or `null` for none.
fn nullable_string(req: &mut Request, bytes: Option<&[u8]>) -> Result<Owner<Value>, Thrown> {
    match bytes {
        Some(b) => string(req, b),
        None => Ok(Owner::null()),
    }
}

/// `php_dom_create_object`: the object standing for `node`, one more owner
/// of it where one exists, a fresh one of the node's class otherwise,
/// sharing `context`'s document.
fn object_for(
    req: &mut Request,
    node: Node,
    context: Option<PhpObject>,
) -> Result<Owner<Value>, Thrown> {
    let parent = context.and_then(|c| libxml_half(req, c));
    made_over(req, node, parent.as_deref(), parent.as_deref())
}

/// `php_dom_create_object` as `xml_common.h` exports it (`PHP_DOM_EXPORT`)
/// to the extensions that include that header, `php_xmlreader.c`'s
/// `expand` through `DOM_RET_OBJ`: the object standing for `node`, one more
/// owner of it where one exists, a fresh one of the node's class sharing
/// `context`'s document otherwise (design §4.10).
///
/// ```php
/// $r = XMLReader::fromString('<r><a x="1">t</a></r>'); $r->read(); $r->read();
/// $n = $r->expand(); var_dump(get_class($n), $n->ownerDocument);
/// $d = new DOMDocument; $d->appendChild($d->createElement('b'));
/// $m = $r->expand($d->documentElement); var_dump($d->saveXML($m), $m->ownerDocument === $d);
/// ```
/// ```expect
/// string(10) "DOMElement"
/// NULL
/// string(14) "<a x="1">t</a>"
/// bool(true)
/// ```
pub fn create_object(
    req: &mut Request,
    node: Node,
    context: Option<PhpObject>,
) -> Result<Owner<Value>, Thrown> {
    object_for(req, node, context)
}

/// `php_dom_create_object(node, rv, (dom_object *) doc)` over a libxml
/// half standing in for a dom object (`xml_common.h`, `PHP_DOM_EXPORT`,
/// design §4.10), as ext/xsl calls it twice: `xsl_proxy_factory` over the
/// half it keeps for one transformation, and `importStylesheet` over the
/// half of the DOM or ext/simplexml object it is handed
/// (`xsltprocessor.c`'s `php_dom_create_object(nodep->doc, ..)`). The
/// object standing for `node`, one more owner of it where one exists, a
/// fresh one of the node's class, mapped by `doc`'s `registerNodeClass`
/// table, counting `doc`'s document otherwise.
pub fn create_object_in(
    req: &mut Request,
    node: Node,
    doc: &NodeObject,
) -> Result<Owner<Value>, Thrown> {
    made_over(req, node, Some(doc), Some(doc))
}

/// `php_dom_create_object(obj, return_value, domobj)` over `domobj`'s
/// libxml half, `parent`: the object standing for `node`, one more owner
/// of it where one exists, otherwise a fresh one of the node's class,
/// the modern family where `parent`'s document is modern, mapped by its
/// `registerNodeClass` table, made by [`instantiate`] counting the node's
/// document under the reference `counted` holds, `parent` where
/// `php_dom_instantiate_object_helper` reads `parent->document` and the
/// evaluation's count of it where a `DOMXPath` node set's objects are made.
fn made_over(
    req: &mut Request,
    node: Node,
    parent: Option<&NodeObject>,
    counted: Option<&NodeObject>,
) -> Result<Owner<Value>, Thrown> {
    if let Some(existing) = node.object() {
        return Ok(Owner::take(req, existing.as_value()));
    }
    let modern = parent.is_some_and(|p| p.class_type() == ClassType::Modern);
    let Some(name) = class_for(node, modern) else {
        let n = node.kind().number();
        let ce = throwables::Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            &format!("Unsupported node type: {n}"),
        ));
    };
    let base = class(req, name)?;
    let class = parent.map_or(base, |p| p.mapped(base));
    instantiate(req, class, node, counted)
}

/// [`made_over`] the object `context`, counting the node's document
/// under the reference `counted` holds: how a `DOMXPath` evaluation makes its
/// node set's objects, the class from the object php's
/// `dom_xpath_intern_for_doc` answers for each node and the document the
/// node's own, which the evaluation holds for the loop.
pub(crate) fn object_counting(
    req: &mut Request,
    node: Node,
    context: PhpObject,
    counted: Option<&NodeObject>,
) -> Result<Owner<Value>, Thrown> {
    let parent = libxml_half(req, context);
    made_over(req, node, parent.as_deref(), counted)
}

/// [`DocOf::under`], or where no reference to the node's document is in
/// reach, the `InternalError` fatal: an object that stood for the node
/// counting another document would outlive the node's.
pub(crate) fn doc_of<'d>(
    req: &mut Request,
    node: Node,
    by: Option<&'d NodeObject>,
) -> Result<DocOf<'d>, Thrown> {
    DocOf::under(node, by).ok_or_else(|| DocOf::uncounted(req))
}

/// `php_dom_create_nullable_object`.
fn nullable_object(
    req: &mut Request,
    node: Option<Node>,
    context: Option<PhpObject>,
) -> Result<Owner<Value>, Thrown> {
    match node {
        Some(n) => object_for(req, n, context),
        None => Ok(Owner::null()),
    }
}

/// `php_dom_instantiate_object_helper`: a fresh object of `class` over
/// `node`, counting the node's own document, under the reference
/// `counted`, a dom object's libxml half, counts where it is that
/// document's. The reference is read once the object is made, as php
/// reads `parent->document` after `object_init_ex`, since making it can
/// serve an autoloader.
fn instantiate(
    req: &mut Request,
    class: ClassId,
    node: Node,
    counted: Option<&NodeObject>,
) -> Result<Owner<Value>, Thrown> {
    let object = new_object(req, class)?;
    let of = match doc_of(req, node, counted) {
        Ok(of) => of,
        Err(thrown) => {
            object.give_up(req);
            return Err(thrown);
        }
    };
    if let Some(mut obj) = libxml_half(req, *object) {
        obj.attach(req, of);
    }
    Ok(object.into_value())
}

/// `dom_objects_free_storage`'s libxml half, past each class's own:
/// the node and the document let go of, what they free of the request's
/// heap given back, before the payload goes.
pub(crate) fn release_libxml(req: &mut Request, object: PhpObject) {
    if let Some(mut obj) = dom_mut(req, object).map(|d| d.obj.borrow()) {
        obj.release(req);
    }
}

/// The object's libxml half, named as the object it is the half of and
/// borrowed for a call that takes the request beside it
/// ([`php_ext_libxml::ffi::NodeObject::borrow`]).
pub(crate) fn libxml_half(req: &mut Request, object: PhpObject) -> Option<Borrowed> {
    dom_mut(req, object).map(|d| {
        d.obj.set_object(object);
        d.obj.borrow()
    })
}

/// A constructor's bind: the node the object stood for let go of, and
/// `node`, which a constructor makes in no document, taken,
/// `php_libxml_increment_node_ptr` with no document.
fn rebind(req: &mut Request, object: PhpObject, node: Node) -> Result<(), Thrown> {
    let of = doc_of(req, node, None)?;
    if let Some(mut obj) = libxml_half(req, object) {
        if obj.is_bound() {
            obj.decrement_resource(req);
        }
        obj.attach(req, of);
    }
    Ok(())
}

/// The slots `dom_object_handlers` overrides, shared by every dom table.
fn read_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    read: PropertyRead,
) -> Result<Owner<Value>, Thrown> {
    match handler(req, object, name) {
        Some(prop) => Ok(match (prop.read)(req, object)? {
            Some(read) => read,
            None => Owner::null(),
        }),
        None => Std.read_property(req, object, name, read),
    }
}

fn write_property(
    req: &mut Request,
    object: PhpObject,
    name: Owner<PhpString>,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    let Some(prop) = handler(req, object, name.as_bytes()) else {
        return Std.write_property(req, object, name, value);
    };
    let bytes = name.as_bytes().to_vec();
    name.give_up(req);
    let Some(write) = prop.write else {
        value.give_up(req);
        return Err(readonly_error(req, object, &bytes));
    };
    let coerced = coerce_property(req, object, &bytes, &value);
    value.give_up(req);
    let coerced = coerced?;
    let written = write(req, object, &coerced);
    coerced.give_up(req);
    written
}

/// `dom_get_property_ptr_ptr`: php's `NULL` for a name a handler
/// serves, so every write to it reaches `dom_write_property`, and the
/// standard place for every other name.
fn property_place(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
) -> Result<PropertyPlace, Thrown> {
    if handler(req, object, name).is_some() {
        return Ok(PropertyPlace::Handlers);
    }
    Std.get_property_ptr_ptr(req, object, name)
}

fn has_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    test: PropertyTest,
) -> Result<bool, Thrown> {
    let Some(prop) = handler(req, object, name) else {
        return Std.has_property(req, object, name, test);
    };
    if test == PropertyTest::Exists {
        return Ok(true);
    }
    let Some(value) = (prop.read)(req, object)? else {
        return Ok(false);
    };
    let answer = match test {
        PropertyTest::NotEmpty => truthy(req, &value),
        _ => Ok(!matches!(value.as_ref(), ValueRef::Null)),
    };
    value.give_up(req);
    answer
}

fn unset_property(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    if handler(req, object, name).is_none() {
        return Std.unset_property(req, object, name);
    }
    let class = class_name(req, object.class());
    let name = String::from_utf8_lossy(name);
    let ce = throwables::Throwables::of(req)?;
    Err(throw_error(
        req,
        ce.error,
        &format!("Cannot unset {class}::${name}"),
    ))
}

/// `dom_get_debug_info_helper`: the object's own properties, then every
/// handled property read with deprecations quiet, an object shown as
/// `(object value omitted)`. A reader that fails is left out and its
/// throwable is left pending beside the table, each later one chained
/// onto it, as php leaves them in `EG(exception)`
/// (`Z/ext/dom/php_dom.c:526-528`), so the dump prints and then throws.
///
/// ```php
/// class Demo extends DOMXPath { public function __construct() {} }
/// try { var_dump(new Demo); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(Demo)#1 (1) {
///   ["registerNodeNamespaces"]=>
///   bool(true)
/// }
/// Invalid State Error
/// ```
///
/// `(object value omitted)` is one string, made before the walk with
/// `ZSTR_INIT_LITERAL` and shared by every row that shows an object, one
/// owner apiece (`object_str`, `:514-541`), and each reader's string is
/// the one its php body makes: `ZVAL_STRING` of libxml's bytes, never
/// the interned one-byte or empty string, and `ZVAL_EMPTY_STRING` where
/// there is nothing, as for `className` and `id` with no attribute.
///
/// ```php
/// $doc = new DOMDocument(); debug_zval_dump($doc->createElement('x'));
/// ```
/// ```expect
/// object(DOMElement)#2 (27) refcount(1){
///   ["tagName"]=>
///   string(1) "x" refcount(1)
///   ["className"]=>
///   string(0) "" interned
///   ["id"]=>
///   string(0) "" interned
///   ["schemaTypeInfo"]=>
///   NULL
///   ["firstElementChild"]=>
///   NULL
///   ["lastElementChild"]=>
///   NULL
///   ["childElementCount"]=>
///   int(0)
///   ["previousElementSibling"]=>
///   NULL
///   ["nextElementSibling"]=>
///   NULL
///   ["nodeName"]=>
///   string(1) "x" refcount(1)
///   ["nodeValue"]=>
///   string(0) "" refcount(1)
///   ["nodeType"]=>
///   int(1)
///   ["parentNode"]=>
///   NULL
///   ["parentElement"]=>
///   NULL
///   ["childNodes"]=>
///   string(22) "(object value omitted)" refcount(3)
///   ["firstChild"]=>
///   NULL
///   ["lastChild"]=>
///   NULL
///   ["previousSibling"]=>
///   NULL
///   ["nextSibling"]=>
///   NULL
///   ["attributes"]=>
///   string(22) "(object value omitted)" refcount(3)
///   ["isConnected"]=>
///   bool(false)
///   ["ownerDocument"]=>
///   string(22) "(object value omitted)" refcount(3)
///   ["namespaceURI"]=>
///   NULL
///   ["prefix"]=>
///   string(0) "" interned
///   ["localName"]=>
///   string(1) "x" refcount(1)
///   ["baseURI"]=>
///   NULL
///   ["textContent"]=>
///   string(0) "" refcount(1)
/// }
/// ```
fn debug_info(req: &mut Request, object: PhpObject) -> Result<Option<DebugInfo>, Thrown> {
    if matches!(Std.get_method(req, object, b"__debuginfo"), Ok(Some(_))) {
        return Std.get_debug_info(req, object);
    }
    // `zend_array_dup(zend_std_get_properties(object))`.
    let own = object.property_table(req)?;
    let own: Vec<(Key, Value)> = walk::rows(req, own).collect();
    let mut pending: Option<Thrown> = None;
    let out = PhpArray::filled(req, |req, out| {
        for (key, value) in own {
            let value = Owner::take(req, value);
            out.set(req, Owner::take(req, key), value)?;
        }
        let Some(table) = dump_table_of(req, object) else {
            return Ok(());
        };
        let omitted = string(req, b"(object value omitted)")?;
        req.state::<State>().suppress_warnings = true;
        let read = prop_rows(req, object, table, out, *omitted, &mut pending);
        req.state::<State>().suppress_warnings = false;
        omitted.give_up(req);
        read
    })?;
    Ok(Some(DebugInfo {
        table: out,
        thrown: pending,
    }))
}

/// The rows `dom_get_debug_info_helper` reads off the property table:
/// each reader's value, an object shown as `omitted`. A reader that
/// fails is left out, its throwable kept in `pending` for the dumper; a
/// fatal or an exit ends the dump, handed on with what was pending.
fn prop_rows(
    req: &mut Request,
    object: PhpObject,
    table: &[&[Prop]],
    out: &mut Owner<PhpArray>,
    omitted: Value,
    pending: &mut Option<Thrown>,
) -> Result<(), Thrown> {
    for prop in table.iter().flat_map(|t| t.iter()) {
        let read = match (prop.read)(req, object) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(thrown) => {
                let thrown = match pending.take() {
                    Some(earlier) => earlier.chain(req, thrown),
                    None => thrown,
                };
                if thrown.exception().is_none() {
                    return Err(thrown);
                }
                *pending = Some(thrown);
                continue;
            }
        };
        let shown = if read.as_object().is_some() {
            read.give_up(req);
            Owner::take(req, omitted)
        } else {
            read
        };
        out.set_str(req, prop.name, shown)?;
    }
    Ok(())
}

/// `DOM_PROP_NODE`: the node, php's "Invalid State Error" where the object
/// stands for none.
fn prop_node(req: &mut Request, object: PhpObject) -> Result<Node, Thrown> {
    match node_of(req, object) {
        Some(node) => Ok(node),
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `dom_node_is_read_only`.
fn is_read_only(node: Node) -> bool {
    match node.kind() {
        Kind::EntityRef
        | Kind::Entity
        | Kind::DocumentType
        | Kind::Notation
        | Kind::Dtd
        | Kind::ElementDecl
        | Kind::AttributeDecl
        | Kind::EntityDecl
        | Kind::NamespaceDecl => true,
        _ => node.doc().is_none(),
    }
}

/// `dom_node_children_valid`.
fn children_valid(node: Node) -> bool {
    !matches!(
        node.kind(),
        Kind::DocumentType
            | Kind::Dtd
            | Kind::Pi
            | Kind::Comment
            | Kind::Text
            | Kind::CData
            | Kind::Notation
    )
}

/// `dom_node_get_node_name_attribute_or_element`: `prefix:name`.
fn qualified_name(node: Node) -> Vec<u8> {
    let name = node.name();
    match node.ns().and_then(Ns::prefix) {
        Some(prefix) => [prefix.as_slice(), b":", name.as_slice()].concat(),
        None => name,
    }
}

/// `php_dom_next_in_tree_order`: the next node of a preorder walk under
/// `base`, entering an element's children.
fn next_in_tree_order(node: Node, base: Node) -> Option<Node> {
    if node.kind() == Kind::Element
        && let Some(child) = node.children()
    {
        return Some(child);
    }
    if let Some(next) = node.next() {
        return Some(next);
    }
    let mut at = node;
    loop {
        at = at.parent()?;
        if at == base {
            return None;
        }
        if let Some(next) = at.next() {
            return Some(next);
        }
    }
}

/// `php_dom_next_in_tree_order(node, base)` with `base` null or given: the
/// next node of a preorder walk, entering an element's children, over the
/// whole tree where `base` is none.
fn next_in_tree_order_from(node: Node, base: Option<Node>) -> Option<Node> {
    if node.kind() == Kind::Element
        && let Some(child) = node.children()
    {
        return Some(child);
    }
    if let Some(next) = node.next() {
        return Some(next);
    }
    let mut at = node;
    loop {
        at = at.parent()?;
        if Some(at) == base {
            return None;
        }
        if let Some(next) = at.next() {
            return Some(next);
        }
    }
}

/// `php_dom_is_node_connected`.
fn is_connected(node: Node) -> bool {
    let mut at = Some(node);
    while let Some(n) = at {
        if n.kind().is_document() {
            return true;
        }
        at = n.parent();
    }
    false
}

/// `php_dom_get_content_into_zval`: a new string of the content, an
/// attribute's value as `RETVAL_STRING_FAST` makes it, so an empty or
/// one-byte value is the interned string, and the interned empty string
/// or `null` where there is none.
fn content_value(
    req: &mut Request,
    node: Node,
    null_on_failure: bool,
) -> Result<Owner<Value>, Thrown> {
    let content = match node.kind() {
        Kind::Text | Kind::CData | Kind::Pi | Kind::Comment => node.content(),
        Kind::Attribute => {
            // `Z/ext/dom/php_dom.c:2368`.
            return Ok(PhpString::fast(req, &node.attr_value())?.into_value());
        }
        _ => node.get_content(),
    };
    match content {
        Some(bytes) => string(req, &bytes),
        None if null_on_failure => Ok(Owner::null()),
        None => empty_string(req),
    }
}

/// `node_list_unlink`: every node of the list some object names unlinked,
/// the rest walked into, an entity reference passed over.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<!DOCTYPE r [<!ENTITY e "X">]><r a="a&e;b"/>');
/// $at = $d->documentElement->getAttributeNode('a');
/// [$first, $ref, $last] = [$at->firstChild, $at->childNodes[1], $at->lastChild];
/// unset($at);
/// $d->documentElement->setAttribute('a', 'new');
/// var_dump($first->data, $ref->nodeName, $ref->parentNode, $last->data, $last->parentNode);
/// $d->loadXML('<!DOCTYPE r [<!ENTITY e "X">]><r a="a&e;b"/>');
/// $last = $d->documentElement->getAttributeNode('a')->lastChild;
/// $d->documentElement->removeAttribute('a');
/// var_dump($last->data, $last->parentNode);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "e"
/// NULL
/// string(1) "b"
/// NULL
/// string(1) "b"
/// NULL
/// ```
fn node_list_unlink(first: Option<Node>) {
    let mut at = first;
    while let Some(node) = at {
        // Read before the unlink clears it, so the walk goes on past a
        // node with a wrapper; an entity reference is passed over, not
        // the end of the list (php 8.5.12, GH-23331).
        let next = node.next();
        if node.object().is_some() {
            node.unlink();
        } else if node.kind() != Kind::EntityRef {
            node_list_unlink(node.children());
            match node.kind() {
                Kind::AttributeDecl
                | Kind::Dtd
                | Kind::DocumentType
                | Kind::EntityDecl
                | Kind::Attribute
                | Kind::Text => {}
                _ => node_list_unlink(node.properties()),
            }
        }
        at = next;
    }
}

/// `dom_remove_all_children`.
fn remove_all_children(req: &mut Request, node: Node) {
    if node.children().is_some() {
        node_list_unlink(node.children());
        Node::free_list(req, node.children());
        node.set_children(None);
        node.set_last(None);
    }
}

/// `dom_reconcile_ns`.
fn reconcile_ns(doc: Option<Node>, node: Node) {
    if node.kind() == Kind::Element {
        reconcile_ns_internal(doc, node, node.parent());
        node.reconciliate_ns(doc);
    }
}

/// `dom_reconcile_ns_list`.
fn reconcile_ns_list(doc: Option<Node>, first: Node, last: Node) {
    reconcile_ns_list_internal(doc, first, last, first.parent());
    let mut at = first;
    loop {
        at.reconciliate_ns(doc);
        if at == last {
            break;
        }
        match at.next() {
            Some(n) => at = n,
            None => break,
        }
    }
}

/// `dom_hierarchy`: whether putting `child` under `parent` would make a
/// cycle (`false`) or not.
fn hierarchy_ok(parent: Node, child: Node) -> bool {
    if child.doc() != parent.doc() {
        return true;
    }
    if child.kind() == Kind::Document {
        return false;
    }
    let mut at = Some(parent);
    while let Some(n) = at {
        if n == child {
            return false;
        }
        at = n.parent();
    }
    true
}

/// `dom_set_document_ref_pointers`: every object standing for a node of
/// the subtree that counts no document counts the node's own, under the
/// reference `by`, the inserting object's libxml half, counts, as
/// [`doc_of`] checks it; nothing where `by` counts none.
fn set_document_ref_pointers(
    req: &mut Request,
    node: Node,
    by: Option<&NodeObject>,
) -> Result<(), Thrown> {
    let Some(by) = by.filter(|b| b.has_document()) else {
        return Ok(());
    };
    let single = |req: &mut Request, n: Node| -> Result<bool, Thrown> {
        if !doc_of(req, n, Some(by))?.share_document(req) {
            return Ok(false);
        }
        if n.kind() == Kind::Element {
            let mut attr = n.properties();
            while let Some(a) = attr {
                set_document_ref_pointers_attr(req, a, by)?;
                attr = a.next();
            }
        }
        Ok(true)
    };
    if !single(req, node)? {
        return Ok(());
    }
    let mut at = node.children();
    while let Some(n) = at {
        if !single(req, n)? {
            break;
        }
        at = next_in_tree_order(n, node);
    }
    Ok(())
}

/// `dom_set_document_ref_pointers_attr`, under the reference `by` counts.
fn set_document_ref_pointers_attr(
    req: &mut Request,
    attr: Node,
    by: &NodeObject,
) -> Result<(), Thrown> {
    doc_of(req, attr, Some(by))?.share_document(req);
    let mut child = attr.children();
    while let Some(c) = child {
        doc_of(req, c, Some(by))?.share_document(req);
        child = c.next();
    }
    Ok(())
}

/// `php_dom_reconcile_attribute_namespace_after_insertion`.
fn reconcile_attribute_namespace(attr: Node) {
    let (Some(ns), Some(parent)) = (attr.ns(), attr.parent()) else {
        return;
    };
    let matching = parent.search_ns(ns.prefix().as_deref());
    match matching {
        Some(m) if m.href() == ns.href() => attr.set_ns(Some(m)),
        _ => {
            if ns.prefix().is_some() {
                parent.reconciliate_ns(parent.doc());
            }
        }
    }
}

/// `dom_clone_node` without a namespace mapper: `xmlDocCopyNode` with
/// php's extended recursion for a shallow element copy, a DTD copied
/// whole.
fn clone_node(node: Node, doc: Option<Node>, recursive: bool) -> Option<Node> {
    if node.kind() == Kind::Dtd {
        return node.copy_dtd(doc);
    }
    let extended = if !recursive && node.kind() == Kind::Element {
        2
    } else {
        i32::from(recursive)
    };
    node.copy(doc, extended)
}

/// `dom_copy_document_ref`: the switches, the class map, the class family
/// and the serialiser carried to a copy's document, whose props
/// `dom_get_doc_props` lays down; the `Err` is the memory limit.
fn copy_document_ref(req: &mut Request, from: PhpObject, to: PhpObject) -> Result<(), Thrown> {
    let (switches, rows, class_type, handlers) = match dom(req, from) {
        Some(d) if d.obj.has_document() => (
            d.obj.props(),
            d.obj.doc_props().and_then(DocProps::class_rows),
            d.obj.class_type(),
            d.obj.document_handlers(),
        ),
        _ => return Ok(()),
    };
    let Some(mut obj) = libxml_half(req, to) else {
        return Ok(());
    };
    if !obj.has_document() {
        return Ok(());
    }
    if let Some(p) = obj.props_mut(req)? {
        p.switches = switches;
        if let Some(rows) = rows {
            p.copy_classmap(req, rows)?;
        }
    }
    obj.set_class_type(class_type);
    obj.set_document_handlers(handlers);
    Ok(())
}

/// `dom_objects_store_clone_obj`: the object cloned with a copy of its
/// node, the copy counting the document it lands in, made through the
/// namespace mapper where the document follows the standard and a copied
/// document given private data of its own; a namespace node is
/// `dom_object_namespace_node_clone_obj`'s, a fresh declaration node on
/// the same element, which the clone keeps alive too, or on none once
/// that element's node is gone.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'xi');
/// file_put_contents($f, '<?xml version="1.0"?><in/>');
/// $d = new DOMDocument;
/// $d->loadXML('<r xmlns:xi="http://www.w3.org/2001/XInclude"><xi:include href="file://' . $f . '" xmlns:l="urn:l"/></r>');
/// $x = new DOMXPath($d);
/// $x->registerNamespace('xi', 'http://www.w3.org/2001/XInclude');
/// $ns = $x->query('namespace::l', $x->query('//xi:include')->item(0))->item(0);
/// echo (clone $ns)->parentNode->nodeName, "\n";
/// $d->xinclude();
/// $c = clone $ns;
/// var_dump($c->nodeName, $c->parentNode, $c->isConnected);
/// unlink($f);
/// ```
/// ```expect
/// xi:include
/// string(7) "xmlns:l"
/// NULL
/// bool(false)
/// ```
fn clone_object(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let node = node_of(req, object);
    if let Some(n) = node
        && n.kind() == Kind::NamespaceDecl
    {
        return clone_namespace_decl(req, object, n);
    }
    let mut payload = crate::object::Dom::default();
    let mut copied = None;
    let mut fresh = None;
    if (instance_of(req, object, "DOMNode") || instance_of(req, object, "Dom\\Node"))
        && let Some(n) = node
    {
        let cloned = if modern::follows_spec(req, object) {
            if n.kind().is_document() {
                let mut private = private::Private::new(req)?;
                let cloned = node::clone_node_modern(req, &mut private.mapper, n, n.doc(), true);
                let cloned = match cloned {
                    Ok(cloned) => cloned,
                    Err(thrown) => {
                        private.free(req);
                        return Err(thrown);
                    }
                };
                fresh = Some(private);
                cloned
            } else {
                match libxml_half(req, object) {
                    Some(mut obj) => match private::of(&mut obj) {
                        Some(p) => node::clone_node_modern(req, &mut p.mapper, n, n.doc(), true)?,
                        None => clone_node(n, n.doc(), true),
                    },
                    None => clone_node(n, n.doc(), true),
                }
            }
        } else {
            clone_node(n, n.doc(), true)
        };
        if let Some(cloned) = cloned {
            // A clone of a document is a document of its own, which takes
            // its first reference.
            let half = libxml_half(req, object).filter(|_| cloned.doc() == n.doc());
            let of = match doc_of(req, cloned, half.as_deref()) {
                Ok(of) => of,
                Err(thrown) => {
                    if let Some(private) = fresh {
                        private.free(req);
                    }
                    return Err(thrown);
                }
            };
            payload.obj.attach(req, of);
            copied = Some((n, cloned));
        }
    }
    let clone = match object.clone_with(req, payload) {
        Ok(clone) => clone,
        Err(thrown) => {
            if let Some(private) = fresh {
                private.free(req);
            }
            return Err(thrown);
        }
    };
    if let Some(d) = dom_mut(req, *clone) {
        d.obj.set_object(*clone);
    }
    if let Some((n, cloned)) = copied
        && cloned.doc() != n.doc()
        && let Err(thrown) = copy_document_ref(req, object, *clone)
    {
        if let Some(private) = fresh {
            private.free(req);
        }
        clone.give_up(req);
        return Err(thrown);
    }
    if let Some(private) = fresh {
        match libxml_half(req, *clone) {
            Some(mut obj) => obj.set_private_data(req, Box::new(private)),
            None => private.free(req),
        }
    }
    Ok(clone)
}

/// [`clone_object`] of a namespace node: a declaration of its own, made
/// under the element the object's owner names, which the clone's owner
/// names too.
fn clone_namespace_decl(
    req: &mut Request,
    object: PhpObject,
    n: Node,
) -> Result<Owner<PhpObject>, Thrown> {
    let mut payload = crate::object::Dom::default();
    let parent_obj = match dom(req, object).map(|d| &d.extra) {
        Some(Extra::NamespaceParent(p)) => Some(**p),
        _ => None,
    };
    let ns = n.ns();
    // The element is the one the object's owner names, which is none
    // once a removal took its node away (`xinclude()` among them),
    // never the declaration's own parent pointer, which can dangle
    // then (php 8.5.12).
    let parent = parent_obj.and_then(|p| node_of(req, p));
    if let Some(cloned) = Node::fake_namespace_decl(
        parent,
        parent.map_or(n.doc(), Node::doc),
        ns.and_then(Ns::href).as_deref(),
        ns.and_then(Ns::prefix).as_deref(),
    ) {
        let half = libxml_half(req, object);
        let of = doc_of(req, cloned, half.as_deref())?;
        payload.obj.attach(req, of);
    }
    if let Some(p) = parent_obj {
        payload.extra = Extra::NamespaceParent(Owner::take(req, p));
    }
    let clone = object.clone_with(req, payload)?;
    if let Some(d) = dom_mut(req, *clone) {
        d.obj.set_object(*clone);
    }
    Ok(clone)
}

/// What `xmlHasProp` or `xmlHasNsProp` answered, where it is an attribute
/// the element carries: never the declaration (`XML_ATTRIBUTE_DECL`) of
/// the default a DTD gives an element that does not carry it, which both
/// answer too.
pub(crate) fn carried(found: Option<Node>) -> Option<Node> {
    found.filter(|a| a.kind() != Kind::AttributeDecl)
}

/// A walk php makes by recursing over the tree, asking at each level
/// whether the stack has come down to its limit
/// (`zend_call_stack_overflowed(EG(stack_limit))`): a level that finds it
/// has does nothing more, the levels above go on, and the native raises
/// the refusal once at the end, as php throws only the first.
pub(crate) struct StackGuard<'r> {
    req: &'r Request,
    reached: bool,
}

impl<'r> StackGuard<'r> {
    pub(crate) fn new(req: &'r Request) -> StackGuard<'r> {
        StackGuard {
            req,
            reached: false,
        }
    }

    /// Whether this level is past the limit, kept for [`StackGuard::reached`].
    pub(crate) fn overflowed(&mut self) -> bool {
        let over = self.req.limits().stack_overflowed();
        self.reached |= over;
        over
    }

    /// Whether any level was.
    pub(crate) fn reached(&self) -> bool {
        self.reached
    }
}

/// What a walk over the tree refuses where it reached the stack's limit:
/// php's sentence for that walk, which normalisation and comparison word
/// apart.
#[derive(Clone, Copy)]
pub(crate) enum StackRefusal {
    /// `dom_normalize_check_stack_limit`.
    Normalize,
    /// `php_dom_node_is_equal_node`.
    Compare,
}

impl StackRefusal {
    /// The walk's end: the refusal raised where a level reached the limit
    /// (`StackGuard::reached`), once, as php throws only the first.
    pub(crate) fn raise_if(self, req: &mut Request, reached: bool) -> Result<(), Thrown> {
        if !reached {
            return Ok(());
        }
        let message = match self {
            StackRefusal::Normalize => "Maximum call stack size reached. Infinite recursion?",
            StackRefusal::Compare => "Maximum call stack size reached.",
        };
        let ce = throwables::Throwables::of(req)?;
        Err(throw_error(req, ce.error, message))
    }
}

/// `php_dom_normalize_legacy`.
fn normalize(guard: &mut StackGuard<'_>, node: Node) {
    if guard.overflowed() {
        return;
    }
    let mut child = node.children();
    while let Some(c) = child {
        match c.kind() {
            Kind::Text => {
                let mut next = c.next();
                while let Some(n) = next {
                    if n.kind() != Kind::Text {
                        break;
                    }
                    let after = n.next();
                    if let Some(content) = n.content() {
                        c.add_content(&content);
                    }
                    n.unlink();
                    if !n.is_named() {
                        n.free();
                    }
                    next = after;
                }
                if c.content().is_none_or(|s| s.is_empty()) {
                    let next = c.next();
                    c.unlink();
                    if !c.is_named() {
                        c.free();
                    }
                    child = next;
                    continue;
                }
            }
            Kind::Element => {
                normalize(guard, c);
                let mut attr = c.properties();
                while let Some(a) = attr {
                    normalize(guard, a);
                    attr = a.next();
                }
            }
            _ => {}
        }
        child = c.next();
    }
}

/// `dom_has_feature`.
fn has_feature(feature: &[u8], version: &[u8]) -> bool {
    matches!(version, b"1.0" | b"2.0" | b"")
        && (feature.eq_ignore_ascii_case(b"XML")
            || (feature.eq_ignore_ascii_case(b"Core") && version == b"1.0"))
}

/// `php_dom_is_equal_attr`.
fn is_equal_attr(a: Node, b: Node) -> bool {
    a.name() == b.name() && ns_uri_equal(a, b) && content_equal(a, b)
}

/// `php_dom_node_is_equal_node`: the legacy classes compare an element's
/// namespace declarations too, a document that follows the standard
/// (`spec_compliant`) keeps none to compare.
/// A level past the stack's limit answers unequal, its refusal raised by
/// the native once the walk ends.
fn is_equal_node(guard: &mut StackGuard<'_>, a: Node, b: Node, spec_compliant: bool) -> bool {
    if guard.overflowed() {
        return false;
    }
    if a.kind() != b.kind() {
        return false;
    }
    match a.kind() {
        Kind::Element => {
            a.name() == b.name()
                && ns_prefix_equal(a, b)
                && ns_uri_equal(a, b)
                && unordered_equal(guard, a.properties(), b.properties(), spec_compliant)
                && (spec_compliant || unordered_ns_equal(a.ns_def(), b.ns_def()))
                && ordered_equal(guard, a.children(), b.children(), spec_compliant)
        }
        Kind::Dtd => {
            a.name() == b.name()
                && a.dtd_external_id() == b.dtd_external_id()
                && a.dtd_system_id() == b.dtd_system_id()
        }
        Kind::Pi => a.name() == b.name() && a.content() == b.content(),
        Kind::Text | Kind::Comment | Kind::CData => a.content() == b.content(),
        Kind::Attribute => is_equal_attr(a, b),
        Kind::EntityRef => a.name() == b.name(),
        Kind::EntityDecl | Kind::Notation | Kind::Entity => {
            a.entity_type() == b.entity_type()
                && a.name() == b.name()
                && a.entity_external_id() == b.entity_external_id()
                && a.entity_system_id() == b.entity_system_id()
                && content_equal(a, b)
        }
        Kind::NamespaceDecl => {
            a.ns().and_then(Ns::prefix) == b.ns().and_then(Ns::prefix)
                && a.ns().and_then(Ns::href) == b.ns().and_then(Ns::href)
        }
        Kind::Fragment | Kind::HtmlDocument | Kind::Document => {
            ordered_equal(guard, a.children(), b.children(), spec_compliant)
        }
        _ => false,
    }
}

/// `dom_get_valid_file_path`: a `file://` URI's path, or a path, made
/// absolute (`VCWD_REALPATH`, else `expand_filepath`); another scheme's
/// URI as it is; `None` where the path does not fit `MAXPATHLEN`, as both
/// of php's resolvers refuse it.
fn file_path(req: &mut Request, source: &[u8]) -> Option<Vec<u8>> {
    let lower: Vec<u8> = source.iter().take(17).map(u8::to_ascii_lowercase).collect();
    let local = if lower.starts_with(b"file://localhost/") {
        &source[16..]
    } else if lower.starts_with(b"file:///") {
        &source[7..]
    } else if has_scheme(source) {
        return Some(source.to_vec());
    } else {
        source
    };
    let resolved = req
        .environment_mut()
        .realpath(local)
        .unwrap_or_else(|| req.environment().absolutize(local));
    let bytes = resolved.to_string_lossy().into_owned().into_bytes();
    (bytes.len() < MAXPATHLEN).then_some(bytes)
}

/// `dom_check_qname`: the local name and the prefix of a qualified name,
/// or the error php's check answers.
fn check_qname(name: &[u8], uri_len: usize) -> Result<(Vec<u8>, Option<Vec<u8>>), Code> {
    if name.is_empty() {
        return Err(Code::Namespace);
    }
    let (local, prefix) = if let Some((local, prefix)) = Name::split2(name) {
        (local, Some(prefix))
    } else {
        if uri_len == 0 {
            return Ok((name.to_vec(), None));
        }
        (name.to_vec(), None)
    };
    if !Name::valid_qname(name) || (prefix.is_some() && uri_len == 0) {
        return Err(Code::Namespace);
    }
    Ok((local, prefix))
}

/// `dom_get_ns`: a declaration of `uri` under `prefix` on the element,
/// the reserved prefixes refused.
fn get_ns(node: Node, uri: &[u8], prefix: Option<&[u8]>) -> Result<Ns, Code> {
    let reserved = match prefix {
        Some(b"xml") => uri != XML_NS,
        Some(b"xmlns") => uri != XMLNS_NS,
        Some(_) => uri == XMLNS_NS,
        None => false,
    };
    if reserved {
        return Err(Code::Namespace);
    }
    get_ns_unchecked(node, uri, prefix).ok_or(Code::Namespace)
}

/// `dom_get_ns_unchecked`: `xmlNewNs`, and a fresh `default`, `default1`
/// ... prefix where the one asked for is taken.
fn get_ns_unchecked(node: Node, uri: &[u8], prefix: Option<&[u8]>) -> Option<Ns> {
    node.new_ns(uri, prefix)
        .or_else(|| resolve_prefix_conflict(node, uri))
}

/// `dom_get_ns_resolve_prefix_conflict`.
fn resolve_prefix_conflict(node: Node, uri: &[u8]) -> Option<Ns> {
    node.doc()?;
    let mut prefix = b"default".to_vec();
    let mut counter = 1;
    while node.search_ns(Some(&prefix)).is_some() {
        if counter > 1000 {
            return None;
        }
        prefix = format!("default{counter}").into_bytes();
        counter += 1;
    }
    node.new_ns(uri, Some(&prefix))
}

/// `dom_validate_and_extract`.
fn validate_and_extract(
    uri: Option<&[u8]>,
    qname: &[u8],
) -> Result<(Vec<u8>, Option<Vec<u8>>), Code> {
    let ns = uri.unwrap_or_default();
    if !Name::valid_qname(qname) {
        return Err(Code::InvalidCharacter);
    }
    let (local, prefix) = match Name::split2(qname) {
        Some((l, p)) => (Some(l), Some(p)),
        None => (None, None),
    };
    let prefix_is = |p: &[u8]| prefix.as_deref() == Some(p);
    if (prefix.is_some() && ns.is_empty())
        || (ns != XML_NS && prefix_is(b"xml"))
        || ((qname == b"xmlns" || prefix_is(b"xmlns")) && ns != XMLNS_NS)
        || (ns == XMLNS_NS && qname != b"xmlns" && !prefix_is(b"xmlns"))
    {
        return Err(Code::Namespace);
    }
    Ok((local.unwrap_or_else(|| qname.to_vec()), prefix))
}

/// `php_dom_adopt_node`: the node moved into `doc`, every object of its
/// subtree counting `doc`'s reference after. The modern classes unlink it,
/// set the tree's document and point its namespaces at `doc`'s mapper,
/// where the legacy ones hand it to `xmlDOMWrapAdoptNode`. The `Err` is
/// the memory limit, met where the mapper makes a
/// declaration, after the subtree is the document's.
///
/// ```php
/// foreach ([fn() => new DOMDocument, fn() => Dom\XMLDocument::createEmpty()] as $make) {
///     $s = $make(); $e = $s->appendChild($s->createElement('e'));
///     $e->setAttribute('a', 'victim');
///     $v = $e->getAttributeNode('a')->firstChild;
///     $t = $make(); $t->appendChild($t->adoptNode($e));
///     unset($t, $s, $e);
///     echo $v->data, ' ', get_class($v->ownerDocument), "\n";
/// }
/// ```
/// ```expect
/// victim DOMDocument
/// victim Dom\XMLDocument
/// ```
fn adopt(req: &mut Request, doc_obj: PhpObject, node: Node, doc: Node) -> Result<bool, Thrown> {
    node.invalidate_document_lists();
    let old = node.doc();
    let mut reconciled = Ok(());
    if old == Some(doc) {
        node.unlink();
    } else {
        invalidate(req, doc_obj);
        if modern::follows_spec(req, doc_obj) {
            node.unlink();
            node.set_tree_doc(Some(doc));
            reconciled = private::with_mapper(req, doc_obj, |mapper, req| {
                mapper.reconcile(req, Some(doc), node)
            })
            .map(drop);
        } else if !node.adopt_into(doc) {
            return Ok(false);
        }
        node.undict(old, doc);
        undict_subtree(node, old, doc);
        let half = libxml_half(req, doc_obj);
        if let Some(by) = half.as_deref().filter(|b| b.has_document()) {
            let transfer = |req: &mut Request, n: Node| -> Result<(), Thrown> {
                doc_of(req, n, Some(by))?.transfer_document(req);
                if n.kind() == Kind::Element {
                    let mut attr = n.properties();
                    while let Some(a) = attr {
                        doc_of(req, a, Some(by))?.transfer_document(req);
                        // An attribute's value nodes a wrapper names go
                        // with it (php 8.5.12, GH-23352).
                        let mut child = a.children();
                        while let Some(c) = child {
                            doc_of(req, c, Some(by))?.transfer_document(req);
                            child = c.next();
                        }
                        attr = a.next();
                    }
                }
                Ok(())
            };
            transfer(req, node)?;
            let mut at = node.children();
            while let Some(n) = at {
                transfer(req, n)?;
                at = crate::next_in_tree_order(n, node);
            }
        }
    }
    reconciled.map(|()| true)
}

/// `php_set_attribute_id`.
fn set_attribute_id(req: &mut Request, object: PhpObject, attr: Node, is_id: bool) {
    if is_id && attr.atype() != XML_ATTRIBUTE_ID {
        attr.set_atype(XML_ATTRIBUTE_ID);
    } else if !is_id && attr.atype() == XML_ATTRIBUTE_ID {
        attr.remove_id();
        attr.set_atype(0);
    }
    mark_ids_modified(req, object);
}

/// The table of a dom class by name, `classes`' entry; `None` for a
/// class php registers none for (`DOMImplementation`).
///
/// `Dom\Notation`'s is `dom_modern_notation_prop_handlers`, its two ids
/// before `Dom\Node`'s rows; the dump alone keeps 8.5.12's rows
/// (`dump_table_of`).
fn table(name: &[u8]) -> Option<&'static [&'static [Prop]]> {
    use crate::props::{
        ATTR, CHARACTER_DATA, DOCUMENT_TYPE, ELEMENT, ENTITY, ENTITY_REFERENCE, MODERN_ATTR,
        MODERN_CHARACTER_DATA, MODERN_DOCUMENT_TYPE, MODERN_ELEMENT, MODERN_ENTITY,
        MODERN_ENTITY_REFERENCE, MODERN_NODE, MODERN_NODE_TEXT_WRITABLE, MODERN_NODE_WRITABLE,
        MODERN_PARENT, MODERN_PROCESSING_INSTRUCTION, MODERN_TEXT, NAMESPACE_NODE, NODE, NOTATION,
        PARENT, PROCESSING_INSTRUCTION, TEXT,
    };
    Some(match name {
        b"DOMNode" => &[NODE],
        b"DOMNameSpaceNode" => &[NAMESPACE_NODE],
        b"DOMDocumentFragment" => &[PARENT, NODE],
        b"DOMDocument" => &[crate::document::PROPS, PARENT, NODE],
        b"DOMNodeList"
        | b"DOMNamedNodeMap"
        | b"Dom\\NodeList"
        | b"Dom\\NamedNodeMap"
        | b"Dom\\DtdNamedNodeMap"
        | b"Dom\\HTMLCollection" => &[crate::map::PROPS],
        b"DOMCharacterData" | b"DOMComment" => &[CHARACTER_DATA, NODE],
        b"DOMAttr" => &[ATTR, NODE],
        b"DOMElement" => &[ELEMENT, NODE],
        b"DOMText" | b"DOMCdataSection" => &[TEXT, CHARACTER_DATA, NODE],
        b"DOMDocumentType" => &[DOCUMENT_TYPE, NODE],
        b"DOMNotation" => &[NOTATION, NODE],
        b"DOMEntity" => &[ENTITY, NODE],
        b"DOMEntityReference" => &[ENTITY_REFERENCE],
        b"DOMProcessingInstruction" => &[PROCESSING_INSTRUCTION, NODE],
        b"DOMXPath" | b"Dom\\XPath" => &[crate::xpath::PROPS],
        b"Dom\\Node" => &[MODERN_NODE],
        b"Dom\\Notation" => &[NOTATION, MODERN_NODE],
        b"Dom\\DocumentFragment" => &[MODERN_PARENT, MODERN_NODE_TEXT_WRITABLE],
        b"Dom\\Document" | b"Dom\\HTMLDocument" => &[crate::html_document::PROPS, MODERN_NODE],
        b"Dom\\XMLDocument" => &[
            crate::document::MODERN_XML_PROPS,
            crate::html_document::PROPS,
            MODERN_NODE,
        ],
        b"Dom\\CharacterData" | b"Dom\\Comment" => &[MODERN_CHARACTER_DATA, MODERN_NODE_WRITABLE],
        b"Dom\\Text" | b"Dom\\CDATASection" => {
            &[MODERN_TEXT, MODERN_CHARACTER_DATA, MODERN_NODE_WRITABLE]
        }
        b"Dom\\ProcessingInstruction" => &[
            MODERN_PROCESSING_INSTRUCTION,
            MODERN_CHARACTER_DATA,
            MODERN_NODE_WRITABLE,
        ],
        b"Dom\\Attr" => &[MODERN_ATTR, MODERN_NODE_WRITABLE],
        b"Dom\\Element" | b"Dom\\HTMLElement" => &[MODERN_ELEMENT, MODERN_NODE_TEXT_WRITABLE],
        b"Dom\\DocumentType" => &[MODERN_DOCUMENT_TYPE, MODERN_NODE],
        b"Dom\\Entity" => &[MODERN_ENTITY, MODERN_NODE],
        b"Dom\\EntityReference" => &[MODERN_ENTITY_REFERENCE],
        b"Dom\\TokenList" => &[crate::token_list::PROPS],
        _ => return None,
    })
}

/// `dom_attr_value_will_change`: an ID attribute taken out of libxml's ID
/// table before its value changes.
fn attr_value_will_change(req: &mut Request, object: PhpObject, attr: Node) {
    if attr.atype() == XML_ATTRIBUTE_ID {
        attr.remove_id();
        attr.set_atype(XML_ATTRIBUTE_ID);
    }
    mark_ids_modified(req, object);
}

/// `dom_entity_reference_fetch_and_sync_declaration`: the reference's
/// children pointed at its declaration, and the declaration.
fn sync_entity_reference(node: Node) -> Option<Node> {
    let entity = node.doc_entity();
    node.set_children(entity);
    node.set_last(entity);
    node.alias_content_of(entity);
    entity
}

/// A fresh list or map of `class` over `base`, `php_dom_create_obj_map`.
fn create(
    req: &mut Request,
    base: PhpObject,
    class_name: &str,
    kind: MapKind,
) -> Result<Owner<Value>, Thrown> {
    let class = class(req, class_name)?;
    let object = php_ext::call::new_object(req, class)?;
    let base = Owner::take(req, base);
    match dom_mut(req, *object) {
        Some(d) => {
            d.extra = Extra::Map(Box::new(Map {
                base: Some(base),
                kind,
                dims: map::Dims::of(class_name),
            }));
        }
        None => base.give_up(req),
    }
    Ok(object.into_value())
}

/// A list of `class_name` over the objects `array` holds, the answer of an
/// `XPath` query; `array` is the list's from here.
fn node_set(
    req: &mut Request,
    class_name: &str,
    array: Owner<PhpArray>,
) -> Result<Owner<Value>, Thrown> {
    let class = match class(req, class_name) {
        Ok(class) => class,
        Err(thrown) => {
            array.give_up(req);
            return Err(thrown);
        }
    };
    let object = match php_ext::call::new_object(req, class) {
        Ok(object) => object,
        Err(thrown) => {
            array.give_up(req);
            return Err(thrown);
        }
    };
    match dom_mut(req, *object) {
        Some(d) => {
            d.extra = Extra::Map(Box::new(Map {
                base: None,
                kind: MapKind::NodeSet(array),
                dims: map::Dims::of(class_name),
            }));
        }
        None => array.give_up(req),
    }
    Ok(object.into_value())
}

fn content_equal(a: Node, b: Node) -> bool {
    a.get_content() == b.get_content()
}

fn ns_prefix_equal(a: Node, b: Node) -> bool {
    a.ns().and_then(Ns::prefix) == b.ns().and_then(Ns::prefix)
}

fn ns_uri_equal(a: Node, b: Node) -> bool {
    a.ns().and_then(Ns::href) == b.ns().and_then(Ns::href)
}

fn ordered_equal(
    guard: &mut StackGuard<'_>,
    a: Option<Node>,
    b: Option<Node>,
    spec_compliant: bool,
) -> bool {
    let (a, b) = (node_list(a), node_list(b));
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| is_equal_node(guard, *x, *y, spec_compliant))
}

fn unordered_equal(
    guard: &mut StackGuard<'_>,
    a: Option<Node>,
    b: Option<Node>,
    spec_compliant: bool,
) -> bool {
    let (a, b) = (node_list(a), node_list(b));
    a.len() == b.len()
        && a.iter().all(|x| {
            b.iter()
                .any(|y| is_equal_node(guard, *x, *y, spec_compliant))
        })
}

fn unordered_ns_equal(a: Option<Ns>, b: Option<Ns>) -> bool {
    let (a, b) = (ns_list(a), ns_list(b));
    a.len() == b.len()
        && a.iter().all(|x| {
            b.iter()
                .any(|y| x.prefix() == y.prefix() && x.href() == y.href())
        })
}

/// `dom_reconcile_ns_internal`.
fn reconcile_ns_internal(doc: Option<Node>, node: Node, search_parent: Option<Node>) {
    let mut prev: Option<Ns> = None;
    let mut cur = node.ns_def();
    while let Some(ns) = cur {
        let next = ns.next();
        let mut kept = true;
        if let (Some(href), Some(parent)) = (ns.href(), search_parent)
            && let Some(found) = parent.search_ns_by_href(doc, &href)
            && (ns.prefix().is_none() || found.prefix() == ns.prefix())
        {
            ns.set_next(None);
            match prev {
                None => node.set_ns_def(next),
                Some(p) => p.set_next(next),
            }
            ns.move_to_old(doc);
            kept = false;
        }
        if kept {
            prev = Some(ns);
        }
        cur = next;
    }
}

fn reconcile_ns_list_internal(
    doc: Option<Node>,
    first: Node,
    last: Node,
    search_parent: Option<Node>,
) {
    let mut at = first;
    loop {
        if at.kind() == Kind::Element {
            reconcile_ns_internal(doc, at, search_parent);
            if let (Some(child), Some(child_last)) = (at.children(), at.last()) {
                reconcile_ns_list_internal(doc, child, child_last, search_parent);
            }
        }
        if at == last {
            break;
        }
        match at.next() {
            Some(n) => at = n,
            None => break,
        }
    }
}

/// `zend_readonly_property_modification_error_ex`.
fn readonly_error(req: &mut Request, object: PhpObject, name: &[u8]) -> Thrown {
    let class = class_name(req, object.class());
    let name = String::from_utf8_lossy(name);
    let ce = match throwables::Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw_error(
        req,
        ce.error,
        &format!("Cannot modify readonly property {class}::${name}"),
    )
}

/// What a handled property's declared type takes, `zend_verify_property_type`
/// under the weak rule, over the types dom's writable properties have: a
/// refusal names the class that declares the property, as php's
/// `zend_verify_property_type_error` names `info->ce`.
///
/// ```php
/// $d = new DOMDocument;
/// try { $d->nodeValue = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $h = Dom\HTMLDocument::createEmpty();
/// try { $h->body = $h->createElementNS('urn:x', 'body'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $h->URL = null; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $h->appendChild($h->createElement('html'));
/// $h->body = $h->createElement('body');
/// var_dump($h->body->tagName);
/// try { $h->body = null; } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot assign array to property DOMNode::$nodeValue of type ?string
/// Cannot assign Dom\Element to property Dom\Document::$body of type ?Dom\HTMLElement
/// Cannot assign null to property Dom\Document::$URL of type string
/// string(4) "BODY"
/// The new body must either be a body or a frameset tag
/// ```
fn coerce_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    value: &Value,
) -> Result<Owner<Value>, Thrown> {
    let (declaring, declared) = declared_type(req, object, name);
    let v = value.deref();
    let given = match v.as_ref() {
        ValueRef::Null => "null",
        ValueRef::Bool(_) => "bool",
        ValueRef::Int(_) => "int",
        ValueRef::Float(_) => "float",
        ValueRef::Str(_) => "string",
        ValueRef::Array(_) => "array",
        _ => "",
    };
    let fail = |req: &mut Request, shown: &str| -> Thrown {
        let class = String::from_utf8_lossy(&declaring).into_owned();
        let prop = String::from_utf8_lossy(name);
        let given = if given.is_empty() {
            v.as_object()
                .map(|o| class_name(req, o.class()))
                .unwrap_or_default()
        } else {
            String::from(given)
        };
        type_error(
            req,
            &format!("Cannot assign {given} to property {class}::${prop} of type {shown}"),
        )
    };
    match declared {
        Declared::Bool => match v.as_ref() {
            ValueRef::Bool(_) => Ok(Owner::take(req, v)),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_) => {
                let truth = truthy(req, &v)?;
                Ok(Owner::bool(truth))
            }
            _ => Err(fail(req, "bool")),
        },
        Declared::String { nullable } => match v.as_ref() {
            ValueRef::Null if nullable => Ok(Owner::null()),
            ValueRef::Str(_) => Ok(Owner::take(req, v)),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
                Ok(to_string(req, &v)?.into_value())
            }
            ValueRef::Object(o) if has_to_string(req, o) => Ok(to_string(req, &v)?.into_value()),
            _ => Err(fail(req, if nullable { "?string" } else { "string" })),
        },
        Declared::Object { class, nullable } => {
            let accepted = match v.as_ref() {
                ValueRef::Null => nullable,
                ValueRef::Object(o) => {
                    lookup_class(req, &class)?.is_some_and(|of| class_extends(req, o.class(), of))
                }
                _ => false,
            };
            if accepted {
                return Ok(Owner::take(req, v));
            }
            let shown = format!(
                "{}{}",
                if nullable { "?" } else { "" },
                String::from_utf8_lossy(&class)
            );
            Err(fail(req, &shown))
        }
    }
}

/// The dom class a class is or extends: the one whose property handlers
/// an object of it reads, `dom_objects_set_class_ex`'s walk.
fn base_name(req: &mut Request, class: ClassId) -> Option<Vec<u8>> {
    req.symbols()
        .lineage(class)
        .map_while(|c| req.symbols().class_decl(c))
        .map(|decl| &decl.name.0)
        .find(|name| table(name).is_some())
        .map(|name| name.to_vec())
}

/// The class a node answers as, `php_dom_create_object`'s switch, before
/// the document's class map is read: the `Dom\\` classes where the object
/// asking follows the spec.
fn class_for(node: Node, modern: bool) -> Option<&'static str> {
    if modern {
        return Some(match node.kind() {
            Kind::Document => "Dom\\XMLDocument",
            Kind::HtmlDocument => "Dom\\HTMLDocument",
            Kind::Dtd | Kind::DocumentType => "Dom\\DocumentType",
            Kind::Element if crate::private::ns_is(node, crate::private::HTML_NS) => {
                "Dom\\HTMLElement"
            }
            Kind::Element => "Dom\\Element",
            Kind::Attribute => "Dom\\Attr",
            Kind::Text => "Dom\\Text",
            Kind::Comment => "Dom\\Comment",
            Kind::Pi => "Dom\\ProcessingInstruction",
            Kind::EntityRef => "Dom\\EntityReference",
            Kind::EntityDecl | Kind::ElementDecl => "Dom\\Entity",
            Kind::CData => "Dom\\CDATASection",
            Kind::Fragment => "Dom\\DocumentFragment",
            Kind::Notation => "Dom\\Notation",
            Kind::NamespaceDecl => "DOMNameSpaceNode",
            _ => return None,
        });
    }
    Some(match node.kind() {
        Kind::Document | Kind::HtmlDocument => "DOMDocument",
        Kind::Dtd | Kind::DocumentType => "DOMDocumentType",
        Kind::Element => "DOMElement",
        Kind::Attribute => "DOMAttr",
        Kind::Text => "DOMText",
        Kind::Comment => "DOMComment",
        Kind::Pi => "DOMProcessingInstruction",
        Kind::EntityRef => "DOMEntityReference",
        Kind::EntityDecl | Kind::ElementDecl => "DOMEntity",
        Kind::CData => "DOMCdataSection",
        Kind::Fragment => "DOMDocumentFragment",
        Kind::Notation => "DOMNotation",
        Kind::NamespaceDecl => "DOMNameSpaceNode",
        _ => return None,
    })
}

/// `dom_get_prop_handler`: the handler of `name` for the object.
fn handler(req: &mut Request, object: PhpObject, name: &[u8]) -> Option<&'static Prop> {
    table_of(req, object)?
        .iter()
        .flat_map(|t| t.iter())
        .find(|p| p.name == name)
}

/// `libxml_fixup_name_and_content_outer` below the node itself.
fn undict_subtree(node: Node, old: Option<Node>, new: Node) {
    if node.kind() == Kind::Element {
        let mut attr = node.properties();
        while let Some(a) = attr {
            a.undict(old, new);
            let mut child = a.children();
            while let Some(c) = child {
                c.undict(old, new);
                child = c.next();
            }
            attr = a.next();
        }
    }
    if matches!(node.kind(), Kind::Element | Kind::Attribute) {
        let mut child = node.children();
        while let Some(c) = child {
            c.undict(old, new);
            undict_subtree(c, old, new);
            child = c.next();
        }
    }
}

/// Whether `source` begins with a URI scheme, `xmlParseURIReference`'s
/// `scheme` field.
fn has_scheme(source: &[u8]) -> bool {
    let Some(colon) = source.iter().position(|b| *b == b':') else {
        return false;
    };
    colon > 0
        && source[0].is_ascii_alphabetic()
        && source[..colon]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn node_list(first: Option<Node>) -> Vec<Node> {
    let mut out = Vec::new();
    let mut at = first;
    while let Some(n) = at {
        out.push(n);
        at = n.next();
    }
    out
}

fn ns_list(first: Option<Ns>) -> Vec<Ns> {
    let mut out = Vec::new();
    let mut at = first;
    while let Some(n) = at {
        out.push(n);
        at = n.next();
    }
    out
}

fn has_to_string(req: &mut Request, object: PhpObject) -> bool {
    matches!(Std.get_method(req, object, b"__tostring"), Ok(Some(_)))
}

/// The declared type of a handled property, as the stub writes it: a
/// bool, a string, or a class (`Dom\Document::$body`'s
/// `?Dom\HTMLElement`), each held as `zend_verify_property_type` holds it
/// under the weak rule.
enum Declared {
    Bool,
    String { nullable: bool },
    Object { class: Vec<u8>, nullable: bool },
}

fn declared_type(req: &mut Request, object: PhpObject, name: &[u8]) -> (Vec<u8>, Declared) {
    use php_ext::decl::Type;
    // `zend_hash_find_ptr(&ce->properties_info, name)` and its
    // `prop_info->ce`.
    let row = req.symbols().property_info_row(object.class(), name);
    if let Some(row) = row
        && let Some(p) = req.symbols().row_property_info(row)
    {
        let declared = match &p.ty {
            Some(Type::Bool) => Declared::Bool,
            Some(Type::Named(class)) => Declared::Object {
                class: class.0.to_vec(),
                nullable: false,
            },
            Some(Type::Nullable(inner)) => match &**inner {
                Type::Named(class) => Declared::Object {
                    class: class.0.to_vec(),
                    nullable: true,
                },
                _ => Declared::String { nullable: true },
            },
            _ => Declared::String { nullable: false },
        };
        return (class_name(req, row.declaring()).into_bytes(), declared);
    }
    (
        class_name(req, object.class()).into_bytes(),
        Declared::String { nullable: true },
    )
}

/// `instanceof_function` over two classes: whether `class` is `of` or
/// extends it.
fn class_extends(req: &mut Request, class: ClassId, of: ClassId) -> bool {
    req.symbols().lineage(class).any(|c| c == of)
}

/// `PHP_DOM_DEPRECATED_PROPERTY`: the deprecation, unless a dumper is
/// reading.
fn deprecated_property(req: &mut Request, message: &str) {
    if !req.state::<State>().suppress_warnings {
        deprecated(req, message);
    }
}

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
mod stub;
mod token_list;
mod xml_serializer;
mod xpath;

use php_ext::abi::{
    Dependency, DependencyKind, Extension, ExtensionBuilder, FromArgument, ObjectHandlers, OrFalse,
    Passed, PropertyPlace, PropertyPurpose, PropertyRead, PropertyTest, Raw, Std,
};
use php_ext::attr::php_class;
use php_ext::call::{lookup_class, new_object};
use php_ext::coerce::{argument, to_string, truthy};
use php_ext::decl::{self, Attribute, AttributeArgument, ConstDecl, ConstExpr};
use php_ext::raising::{
    Level, Thrown, arg_count_error, argument_error, deprecated, fatal, new_throwable, throw,
    throw_value,
};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{DocProps, Kind, Name, Node, Ns, SharedDoc, XML_ATTRIBUTE_ID};

use document::{XML_NS, XMLNS_NS};
use map::{Map, MapKind};
use object::{Code, Dom, Extra, State};
use props::Prop;

/// Core's interfaces the lists implement and hand out, named for
/// `implements` and the types that hold them.
#[php_class(extern)]
pub(crate) struct IteratorAggregate;

#[php_class(extern)]
pub(crate) struct Countable;

#[php_class(extern)]
pub(crate) struct Iterator;

/// A native's entry, the shape `ExtensionBuilder` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// A property read's answer: `None` is php's `FAILURE`, a row a dumper
/// leaves out.
type Read = Result<Option<Value>, Thrown>;

/// The legacy classes as `php_dom.stub.php` declares them, in the order
/// `PHP_MINIT_FUNCTION(dom)` registers them.
const STUB: &str = r"
final class DOMException extends Exception
{
    /**
     * Intentionally left untyped for BC reasons
     * @var int
     */
    public $code = 0;
}
/** @not-serializable */
class DOMXPath
{
    public DOMDocument $document;
    public bool $registerNodeNamespaces;

    public function __construct(DOMDocument $document, bool $registerNodeNS = true) {}

    /** @tentative-return-type */
    public function evaluate(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed {}

    /** @tentative-return-type */
    public function query(string $expression, ?DOMNode $contextNode = null, bool $registerNodeNS = true): mixed {}

    /** @tentative-return-type */
    public function registerNamespace(string $prefix, string $namespace): bool {}

    /** @tentative-return-type */
    public function registerPhpFunctions(string|array|null $restrict = null): void {}

    public function registerPhpFunctionNS(string $namespaceURI, string $name, callable $callable): void {}

    public static function quote(string $str): string {}
}
";

/// The `Dom\` namespace as `php_dom.stub.php` declares it (8.4), with the
/// docblocks the reader takes nothing from left out.
const MODERN_STUB: &str = r"
namespace Dom
{
/** @not-serializable */
final class XPath
{
    public Document $document;
    public bool $registerNodeNamespaces;
    public function __construct(Document $document, bool $registerNodeNS = true) {}
    public function evaluate(string $expression, ?Node $contextNode = null, bool $registerNodeNS = true): null|bool|float|string|NodeList {}
    public function query(string $expression, ?Node $contextNode = null, bool $registerNodeNS = true): NodeList {}
    /** @implementation-alias DOMXPath::registerNamespace */
    public function registerNamespace(string $prefix, string $namespace): bool {}
    /** @implementation-alias DOMXPath::registerPhpFunctions */
    public function registerPhpFunctions(string|array|null $restrict = null): void {}
    /** @implementation-alias DOMXPath::registerPhpFunctionNS */
    public function registerPhpFunctionNS(string $namespaceURI, string $name, callable $callable): void {}
    /** @implementation-alias DOMXPath::quote */
    public static function quote(string $str): string {}
}
}
";

/// The order `PHP_MINIT_FUNCTION(dom)` registers the classes of both
/// namespaces in, which `get_declared_classes()` shows.
const MINIT_ORDER: [&str; 50] = [
    "Dom\\AdjacentPosition",
    "DOMException",
    "DOMParentNode",
    "Dom\\ParentNode",
    "DOMChildNode",
    "Dom\\ChildNode",
    "DOMImplementation",
    "Dom\\Implementation",
    "DOMNode",
    "Dom\\Node",
    "DOMNameSpaceNode",
    "Dom\\NamespaceInfo",
    "DOMDocumentFragment",
    "Dom\\DocumentFragment",
    "Dom\\Document",
    "DOMDocument",
    "Dom\\HTMLDocument",
    "Dom\\XMLDocument",
    "DOMNodeList",
    "Dom\\NodeList",
    "DOMNamedNodeMap",
    "Dom\\NamedNodeMap",
    "Dom\\DtdNamedNodeMap",
    "Dom\\HTMLCollection",
    "DOMCharacterData",
    "Dom\\CharacterData",
    "DOMAttr",
    "Dom\\Attr",
    "DOMElement",
    "Dom\\Element",
    "Dom\\HTMLElement",
    "DOMText",
    "Dom\\Text",
    "DOMComment",
    "Dom\\Comment",
    "DOMCdataSection",
    "Dom\\CDATASection",
    "DOMDocumentType",
    "Dom\\DocumentType",
    "DOMNotation",
    "Dom\\Notation",
    "DOMEntity",
    "Dom\\Entity",
    "DOMEntityReference",
    "Dom\\EntityReference",
    "DOMProcessingInstruction",
    "Dom\\ProcessingInstruction",
    "DOMXPath",
    "Dom\\XPath",
    "Dom\\TokenList",
];

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
pub fn extension() -> Extension {
    let entries = entries();
    let mut builder = ExtensionBuilder::default()
        .functions(vec![
            (
                stub::function("dom_import_simplexml(object $node): DOMAttr|DOMElement"),
                import_simplexml as Entry,
            ),
            (
                stub::function("Dom\\import_simplexml(object $node): Dom\\Attr|Dom\\Element"),
                modern::import_simplexml as Entry,
            ),
        ])
        .constants(constants())
        .dependencies(vec![
            dependency("libxml", DependencyKind::Required),
            dependency("lexbor", DependencyKind::Required),
            dependency("domxml", DependencyKind::Conflicts),
        ]);
    let mut declared = stub::classes(STUB, &entries);
    declared.extend(stub::classes(MODERN_STUB, &entries));
    for name in MINIT_ORDER {
        builder = match name {
            "DOMNode" => builder.class::<node::DOMNode>(),
            "DOMNameSpaceNode" => builder.class::<node::DOMNameSpaceNode>(),
            "Dom\\Node" => builder.class::<modern::Node>(),
            "Dom\\NamespaceInfo" => builder.class::<modern::NamespaceInfo>(),
            "DOMParentNode" => builder.class::<parent::DOMParentNode>(),
            "Dom\\ParentNode" => builder.class::<parent::ParentNode>(),
            "DOMChildNode" => builder.class::<parent::DOMChildNode>(),
            "Dom\\ChildNode" => builder.class::<parent::ChildNode>(),
            "Dom\\AdjacentPosition" => builder.class::<element::AdjacentPosition>(),
            "DOMElement" => builder.class::<element::DOMElement>(),
            "Dom\\Element" => builder.class::<element::Element>(),
            "Dom\\HTMLElement" => builder.class::<element::HTMLElement>(),
            "DOMImplementation" => builder.class::<classes::DOMImplementation>(),
            "Dom\\Implementation" => builder.class::<classes::Implementation>(),
            "DOMDocumentFragment" => builder.class::<classes::DOMDocumentFragment>(),
            "Dom\\DocumentFragment" => builder.class::<classes::DocumentFragment>(),
            "DOMCharacterData" => builder.class::<classes::DOMCharacterData>(),
            "Dom\\CharacterData" => builder.class::<classes::CharacterData>(),
            "DOMAttr" => builder.class::<classes::DOMAttr>(),
            "Dom\\Attr" => builder.class::<modern::Attr>(),
            "DOMText" => builder.class::<classes::DOMText>(),
            "Dom\\Text" => builder.class::<classes::Text>(),
            "DOMComment" => builder.class::<classes::DOMComment>(),
            "Dom\\Comment" => builder.class::<classes::Comment>(),
            "DOMCdataSection" => builder.class::<classes::DOMCdataSection>(),
            "Dom\\CDATASection" => builder.class::<classes::CDATASection>(),
            "DOMDocumentType" => builder.class::<classes::DOMDocumentType>(),
            "Dom\\DocumentType" => builder.class::<classes::DocumentType>(),
            "DOMNotation" => builder.class::<classes::DOMNotation>(),
            "Dom\\Notation" => builder.class::<classes::Notation>(),
            "DOMEntity" => builder.class::<classes::DOMEntity>(),
            "Dom\\Entity" => builder.class::<classes::Entity>(),
            "DOMEntityReference" => builder.class::<classes::DOMEntityReference>(),
            "Dom\\EntityReference" => builder.class::<classes::EntityReference>(),
            "DOMProcessingInstruction" => builder.class::<classes::DOMProcessingInstruction>(),
            "Dom\\ProcessingInstruction" => builder.class::<classes::ProcessingInstruction>(),
            "DOMDocument" => builder.class::<document::DOMDocument>(),
            "Dom\\Document" => builder.class::<document::Document>(),
            "Dom\\HTMLDocument" => builder.class::<html_document::HTMLDocument>(),
            "Dom\\XMLDocument" => builder.class::<document::XMLDocument>(),
            "DOMNodeList" => builder.class::<map::DOMNodeList>(),
            "Dom\\NodeList" => builder.class::<map::NodeList>(),
            "DOMNamedNodeMap" => builder.class::<map::DOMNamedNodeMap>(),
            "Dom\\NamedNodeMap" => builder.class::<map::NamedNodeMap>(),
            "Dom\\DtdNamedNodeMap" => builder.class::<map::DtdNamedNodeMap>(),
            "Dom\\HTMLCollection" => builder.class::<map::HTMLCollection>(),
            "Dom\\TokenList" => builder.class::<token_list::TokenList>(),
            _ => builder,
        };
        let Some(at) = declared
            .iter()
            .position(|(d, _)| &*d.name.0 == name.as_bytes())
        else {
            continue;
        };
        let (decl, methods) = declared.remove(at);
        let handlers: Option<&'static dyn ObjectHandlers> = match decl.name.0.as_ref() {
            b"DOMException" => None,
            _ => Some(&xpath::HANDLERS),
        };
        builder = match handlers {
            Some(h) => builder.class_payload::<object::Dom>(decl, methods, h),
            None => builder.classes(vec![(decl, methods)]),
        };
    }
    builder
        .request_state::<object::State>()
        .rinit(register_alias)
        .info(dom_info)
        .build("dom")
}

/// The entries the stub reader's classes name, `Class::method` to entry,
/// until each class is declared by the attributes.
fn entries() -> Vec<(&'static str, Entry)> {
    xpath::METHODS.to_vec()
}

/// `zend_register_class_alias("Dom\\DOMException", ...)` of
/// `register_class_DOMException`: `DOMException` under the modern
/// namespace too.
///
/// ```php
/// var_dump(new Dom\DOMException('m') instanceof DOMException, class_exists('Dom\DOMException'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
fn register_alias(req: &mut Request) -> Result<(), Thrown> {
    if let Some(class) = lookup_class(req, b"DOMException")? {
        req.class_alias(class, b"dom\\domexception");
    }
    Ok(())
}

fn dependency(name: &str, kind: DependencyKind) -> Dependency {
    Dependency {
        name: String::from(name),
        kind,
        rel: None,
        version: None,
    }
}

/// `DOM_HTML_NO_DEFAULT_NS`, the option that keeps the HTML5 parser's
/// elements out of the HTML namespace.
pub(crate) const HTML_NO_DEFAULT_NS: i64 = 1 << 31;

/// `register_php_dom_symbols`: the node types, the attribute types and the
/// error codes, `DOM_PHP_ERR` deprecated since 8.4, then the `Dom\`
/// codes and `Dom\HTML_NO_DEFAULT_NS`.
fn constants() -> Vec<ConstDecl> {
    let rows: [(&[u8], i64); 29] = [
        (b"XML_ELEMENT_NODE", 1),
        (b"XML_ATTRIBUTE_NODE", 2),
        (b"XML_TEXT_NODE", 3),
        (b"XML_CDATA_SECTION_NODE", 4),
        (b"XML_ENTITY_REF_NODE", 5),
        (b"XML_ENTITY_NODE", 6),
        (b"XML_PI_NODE", 7),
        (b"XML_COMMENT_NODE", 8),
        (b"XML_DOCUMENT_NODE", 9),
        (b"XML_DOCUMENT_TYPE_NODE", 10),
        (b"XML_DOCUMENT_FRAG_NODE", 11),
        (b"XML_NOTATION_NODE", 12),
        (b"XML_HTML_DOCUMENT_NODE", 13),
        (b"XML_DTD_NODE", 14),
        (b"XML_ELEMENT_DECL_NODE", 15),
        (b"XML_ATTRIBUTE_DECL_NODE", 16),
        (b"XML_ENTITY_DECL_NODE", 17),
        (b"XML_NAMESPACE_DECL_NODE", 18),
        (b"XML_LOCAL_NAMESPACE", 18),
        (b"XML_ATTRIBUTE_CDATA", 1),
        (b"XML_ATTRIBUTE_ID", 2),
        (b"XML_ATTRIBUTE_IDREF", 3),
        (b"XML_ATTRIBUTE_IDREFS", 4),
        (b"XML_ATTRIBUTE_ENTITY", 6),
        (b"XML_ATTRIBUTE_NMTOKEN", 7),
        (b"XML_ATTRIBUTE_NMTOKENS", 8),
        (b"XML_ATTRIBUTE_ENUMERATION", 9),
        (b"XML_ATTRIBUTE_NOTATION", 10),
        (b"DOM_PHP_ERR", 0),
    ];
    let codes = Code::ALL.iter().map(|c| (c.constant(), *c as i64));
    let modern = Code::ALL
        .iter()
        .filter_map(|c| Some((c.modern_constant()?, *c as i64)));
    let no_default_ns: (&[u8], i64) = (b"Dom\\HTML_NO_DEFAULT_NS", HTML_NO_DEFAULT_NS);
    rows.iter()
        .copied()
        .chain(codes)
        .chain(modern)
        .chain(std::iter::once(no_default_ns))
        .map(|(name, value)| ConstDecl {
            name: decl::Name(name.into()),
            value: ConstExpr::long(value),
            attributes: if name == b"DOM_PHP_ERR" {
                vec![Attribute {
                    name: decl::Name(b"Deprecated".as_slice().into()),
                    arguments: vec![
                        AttributeArgument {
                            name: Some(decl::Name(b"since".as_slice().into())),
                            value: ConstExpr::Str(b"8.4".as_slice().into()),
                        },
                        AttributeArgument {
                            name: Some(decl::Name(b"message".as_slice().into())),
                            value: ConstExpr::Str(b"as it is no longer used".as_slice().into()),
                        },
                    ],
                }]
            } else {
                Vec::new()
            },
        })
        .collect()
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
    req.info_table(&[
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

/// `dom_import_simplexml(object $node): DOMAttr|DOMElement`: a dom
/// element or attribute answers as itself, as php's export of a dom node
/// hands it back; there is no simplexml here.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// var_dump(dom_import_simplexml($d->documentElement) === $d->documentElement);
/// try { dom_import_simplexml(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// dom_import_simplexml(): Argument #1 ($node) is not a valid node type
/// ```
fn import_simplexml(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let object = object_arg(req, 1)?;
    let node = object
        .filter(|o| instance_of(req, *o, "DOMNode"))
        .and_then(|o| node_of(req, o).map(|n| (o, n)));
    match node {
        Some((o, n))
            if matches!(
                n.kind(),
                php_ext_libxml::ffi::Kind::Element | php_ext_libxml::ffi::Kind::Attribute
            ) =>
        {
            object_for(req, n, Some(o))
        }
        _ => Err(argument_error(
            req,
            "TypeError",
            1,
            "is not a valid node type",
        )),
    }
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

/// The document reference an object counts, to share with another.
fn shared_doc(req: &Request, object: PhpObject) -> Option<SharedDoc> {
    dom(req, object).and_then(|d| d.obj.shared_doc())
}

/// `dom_get_doc_props_read_only(obj->document)`.
fn props_of(req: &Request, object: PhpObject) -> DocProps {
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
fn class_name(req: &Request, class: ClassId) -> String {
    req.class_decl(class)
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
        let exception = new_throwable(req, "DOMException", message)?;
        if let Some(object) = exception.as_object().copied() {
            let name = PhpString::new(req, b"code")?;
            object.set_prop(req, name, Value::int(code as i64))?;
        }
        let thrown = throw_value(req, &exception);
        req.release(exception);
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

/// `$this` of the method being served.
fn this(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(fatal(
            req,
            "InternalError: a dom method served without its receiver",
        )),
    }
}

/// `DOM_GET_OBJ`: the node an object stands for, php's `Error` "Couldn't
/// fetch" where it stands for none.
fn fetch(req: &mut Request, object: PhpObject) -> Result<Node, Thrown> {
    if let Some(node) = node_of(req, object) {
        return Ok(node);
    }
    let name = class_name(req, object.class());
    Err(throw(req, "Error", &format!("Couldn't fetch {name}")))
}

/// `DOM_GET_INTERN` of the receiver: php's "Couldn't fetch" where its
/// payload stands for nothing.
fn intern(req: &mut Request, this: PhpObject) -> Result<PhpObject, Thrown> {
    let bound = dom(req, this).is_some_and(|d| d.obj.is_bound() || !matches!(d.extra, Extra::None));
    if bound {
        return Ok(this);
    }
    let name = class_name(req, this.class());
    Err(throw(req, "Error", &format!("Couldn't fetch {name}")))
}

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arity(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (how, n) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let word = if n == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("{how} {n} {word}, {given} given"),
    ))
}

/// The argument at `position` held to its declared type, `None` where the
/// call gave none.
fn arg(req: &mut Request, position: u32) -> Result<Option<Value>, Thrown> {
    argument(req, position)
}

/// `Z_PARAM_STR`: the bytes, empty where the call gave none.
fn bytes_arg(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
        .unwrap_or_default())
}

/// `Z_PARAM_BOOL`, `default` where the call gave none.
fn bool_arg(req: &mut Request, position: u32, default: bool) -> Result<bool, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_bool())
        .unwrap_or(default))
}

/// `Z_PARAM_OBJECT_OF_CLASS[_OR_NULL]`: the object, the declared class
/// already held by the argument's type.
fn object_arg(req: &mut Request, position: u32) -> Result<Option<PhpObject>, Thrown> {
    Ok(arg(req, position)?.and_then(|v| v.as_object().copied()))
}

/// `Z_PARAM_PATH`: the bytes, php's `ValueError` where they hold a NUL.
fn path_arg(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let bytes = bytes_arg(req, position)?;
    if bytes.contains(&0) {
        return Err(php_ext::raising::arg_value_error(
            req,
            position,
            "not contain any null bytes",
        ));
    }
    Ok(bytes)
}

/// `zend_hash_str_find_deref(array, name)`: a view of the element, no
/// owner taken.
fn array_get(req: &mut Request, array: PhpArray, name: &[u8]) -> Result<Option<Value>, Thrown> {
    let key = PhpString::new(req, name)?;
    let found = array.get(&Key::Str(key)).map(|v| v.deref());
    req.release(Value::string(key));
    Ok(found)
}

/// A string value of `bytes`, one owner the caller's.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// A body's value returned under its declared type `T`, as php returns
/// one: php verifies a native's return in its debug builds only.
fn returned<T: FromArgument>(req: &mut Request, value: Value) -> Result<Raw<T>, Thrown> {
    <Raw<T> as FromArgument>::from_value(req, value)
}

/// A body's `T|false` returned under its declared type: `false` as
/// itself, anything else under `T`.
fn or_false<T: FromArgument>(req: &mut Request, value: Value) -> Result<OrFalse<Raw<T>>, Thrown> {
    if value.as_bool() == Some(false) {
        return Ok(OrFalse::False);
    }
    returned(req, value).map(OrFalse::Value)
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
fn nullable_string(req: &mut Request, bytes: Option<&[u8]>) -> Result<Value, Thrown> {
    match bytes {
        Some(b) => string(req, b),
        None => Ok(Value::null()),
    }
}

/// `php_dom_create_object`: the object standing for `node`, one more owner
/// of it where one exists, a fresh one of the node's class otherwise,
/// sharing `context`'s document.
fn object_for(req: &mut Request, node: Node, context: Option<PhpObject>) -> Result<Value, Thrown> {
    if let Some(existing) = node.object() {
        let value = existing.as_value();
        req.addref(&value);
        return Ok(value);
    }
    let modern = context.is_some_and(|c| modern::follows_spec(req, c));
    let Some(name) = class_for(node, modern) else {
        let n = node.kind().number();
        return Err(throw(req, "Error", &format!("Unsupported node type: {n}")));
    };
    let base = class(req, name)?;
    let class = match context {
        Some(ctx) => mapped(&props_of(req, ctx), base),
        None => base,
    };
    instantiate(req, class, node, context)
}

/// `php_dom_create_nullable_object`.
fn nullable_object(
    req: &mut Request,
    node: Option<Node>,
    context: Option<PhpObject>,
) -> Result<Value, Thrown> {
    match node {
        Some(n) => object_for(req, n, context),
        None => Ok(Value::null()),
    }
}

/// `php_dom_instantiate_object_helper`: an object of `class` for `node`,
/// counting `parent`'s document where the node has one.
fn instantiate(
    req: &mut Request,
    class: ClassId,
    node: Node,
    parent: Option<PhpObject>,
) -> Result<Value, Thrown> {
    let object = new_object(req, class)?;
    let shared = parent.and_then(|p| shared_doc(req, p));
    if let Some(d) = dom_mut(req, object) {
        d.obj.set_object(object);
        d.obj.attach(node, shared);
    }
    Ok(object.as_value())
}

/// A constructor's bind: the node the object stood for let go of, and
/// `node` taken, `php_libxml_increment_node_ptr` with no document.
fn rebind(req: &mut Request, object: PhpObject, node: Node) {
    if let Some(d) = dom_mut(req, object) {
        d.obj.set_object(object);
        if d.obj.is_bound() {
            d.obj.decrement_resource();
        }
        d.obj.attach(node, None);
    }
}

/// The slots `dom_object_handlers` overrides, shared by every dom table.
fn read_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    read: PropertyRead,
) -> Result<Value, Thrown> {
    match handler(req, object, name) {
        Some(prop) => Ok((prop.read)(req, object)?.unwrap_or_default()),
        None => Std.read_property(req, object, name, read),
    }
}

fn write_property(
    req: &mut Request,
    object: PhpObject,
    name: PhpString,
    value: Value,
) -> Result<(), Thrown> {
    let Some(prop) = handler(req, object, name.as_bytes()) else {
        return Std.write_property(req, object, name, value);
    };
    let bytes = name.as_bytes().to_vec();
    req.release(Value::string(name));
    let Some(write) = prop.write else {
        req.release(value);
        return Err(readonly_error(req, object, &bytes));
    };
    let coerced = coerce_property(req, object, &bytes, &value);
    req.release(value);
    let coerced = coerced?;
    let written = write(req, object, &coerced);
    req.release(coerced);
    written
}

/// `dom_get_property_ptr_ptr`: php's `NULL` for a name a handler
/// serves, so every write to it reaches `dom_write_property`, and the
/// standard place for every other name.
fn property_place(req: &mut Request, object: PhpObject, name: &[u8]) -> PropertyPlace {
    if handler(req, object, name).is_some() {
        return PropertyPlace::Handlers;
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
        _ => !matches!(value.as_ref(), ValueRef::Null),
    };
    req.release(value);
    Ok(answer)
}

fn unset_property(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    if handler(req, object, name).is_none() {
        return Std.unset_property(req, object, name);
    }
    let class = class_name(req, object.class());
    let name = String::from_utf8_lossy(name);
    Err(throw(
        req,
        "Error",
        &format!("Cannot unset {class}::${name}"),
    ))
}

/// `dom_get_debug_info_helper`: the object's own properties, then every
/// handled property read with deprecations quiet, an object shown as
/// `(object value omitted)`.
fn debug_info(req: &mut Request, object: PhpObject) -> Result<Option<PhpArray>, Thrown> {
    if Std.get_method(req, object, b"__debuginfo").is_some() {
        return Std.get_debug_info(req, object);
    }
    let own = object.props_for(req, PropertyPurpose::ArrayCast)?;
    let mut out = PhpArray::new(req)?;
    for (name, value) in own {
        req.addref(&value);
        out.set(req, Key::Str(name), value)?;
    }
    let Some(table) = table_of(req, object) else {
        return Ok(Some(out));
    };
    req.state::<State>().suppress_warnings = true;
    for prop in table.iter().flat_map(|t| t.iter()) {
        // A reader that fails is left out, as `dom_get_debug_info_helper`
        // leaves it out. php leaves its exception pending, to be thrown
        // once the dump is printed; a handler here cannot, so it is
        // cleared, and one that cannot be cleared ends the dump.
        let read = match (prop.read)(req, object) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(thrown) => {
                if let Err(thrown) = php_ext::raising::clear(req, thrown) {
                    req.state::<State>().suppress_warnings = false;
                    req.release(Value::array(out));
                    return Err(thrown);
                }
                continue;
            }
        };
        let shown = if read.as_object().is_some() {
            req.release(read);
            string(req, b"(object value omitted)")?
        } else {
            read
        };
        let key = Key::Str(PhpString::new(req, prop.name)?);
        out.set(req, key, shown)?;
    }
    req.state::<State>().suppress_warnings = false;
    Ok(Some(out))
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

/// `php_dom_get_content_into_zval`.
fn content_value(req: &mut Request, node: Node, null_on_failure: bool) -> Result<Value, Thrown> {
    let content = match node.kind() {
        Kind::Text | Kind::CData | Kind::Pi | Kind::Comment => node.content(),
        Kind::Attribute => Some(node.attr_value()),
        _ => node.get_content(),
    };
    match content {
        Some(bytes) => string(req, &bytes),
        None if null_on_failure => Ok(Value::null()),
        None => string(req, b""),
    }
}

/// `node_list_unlink`: every node of the list some object names unlinked,
/// the rest walked into.
fn node_list_unlink(first: Option<Node>) {
    let mut at = first;
    while let Some(node) = at {
        if node.object().is_some() {
            node.unlink();
        } else {
            if node.kind() == Kind::EntityRef {
                break;
            }
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
        at = node.next();
    }
}

/// `dom_remove_all_children`.
fn remove_all_children(node: Node) {
    if node.children().is_some() {
        node_list_unlink(node.children());
        Node::free_list(node.children());
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
/// the subtree that counts no document counts `document`.
fn set_document_ref_pointers(node: Node, document: Option<SharedDoc>) {
    let Some(document) = document else {
        return;
    };
    let single = |n: Node| -> bool {
        if !n.share_document(document) {
            return false;
        }
        if n.kind() == Kind::Element {
            let mut attr = n.properties();
            while let Some(a) = attr {
                set_document_ref_pointers_attr(a, document);
                attr = a.next();
            }
        }
        true
    };
    if !single(node) {
        return;
    }
    let mut at = node.children();
    while let Some(n) = at {
        if !single(n) {
            break;
        }
        at = next_in_tree_order(n, node);
    }
}

/// `dom_set_document_ref_pointers_attr`.
fn set_document_ref_pointers_attr(attr: Node, document: SharedDoc) {
    attr.share_document(document);
    let mut child = attr.children();
    while let Some(c) = child {
        c.share_document(document);
        child = c.next();
    }
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

/// `dom_copy_document_ref`: the switches and the class map carried to a
/// copy's document.
fn copy_document_ref(req: &mut Request, from: PhpObject, to: PhpObject) {
    let (props, class_type) = match dom(req, from) {
        Some(d) if d.obj.has_document() => (d.obj.props(), d.obj.class_type()),
        _ => return,
    };
    if let Some(d) = dom_mut(req, to) {
        if let Some(p) = d.obj.props_mut() {
            *p = props;
        }
        d.obj.set_class_type(class_type);
    }
}

/// `dom_objects_store_clone_obj`: the object cloned with a copy of its
/// node, the copy counting the document it lands in, made through the
/// namespace mapper where the document follows the standard and a copied
/// document given private data of its own; a namespace node is
/// `dom_object_namespace_node_clone_obj`'s, a fresh declaration node on
/// the same element, which the clone keeps alive too.
fn clone_object(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let node = node_of(req, object);
    let mut payload = crate::object::Dom::default();
    let mut copied = None;
    if let Some(n) = node
        && n.kind() == Kind::NamespaceDecl
    {
        let parent_obj = match dom(req, object).map(|d| &d.extra) {
            Some(Extra::NamespaceParent(p)) => Some(*p),
            _ => None,
        };
        if let Some(p) = parent_obj {
            req.addref(&p.as_value());
            payload.extra = Extra::NamespaceParent(p);
        }
        let ns = n.ns();
        if let Some(parent) = n.parent()
            && let Some(cloned) = Node::fake_namespace_decl(
                parent,
                ns.and_then(Ns::href).as_deref(),
                ns.and_then(Ns::prefix).as_deref(),
            )
        {
            payload.obj.attach(cloned, shared_doc(req, object));
        }
        let clone = object.clone_with(req, payload)?;
        if let Some(d) = dom_mut(req, clone) {
            d.obj.set_object(clone);
        }
        return Ok(clone);
    }
    let mut fresh = None;
    if (instance_of(req, object, "DOMNode") || instance_of(req, object, "Dom\\Node"))
        && let Some(n) = node
    {
        let cloned = if modern::follows_spec(req, object) {
            if n.kind().is_document() {
                let mut private = private::Private::default();
                let cloned = node::clone_node_modern(&mut private.mapper, n, n.doc(), true);
                fresh = Some(private);
                cloned
            } else {
                match dom_mut(req, object).and_then(|d| private::of(&mut d.obj)) {
                    Some(p) => node::clone_node_modern(&mut p.mapper, n, n.doc(), true),
                    None => clone_node(n, n.doc(), true),
                }
            }
        } else {
            clone_node(n, n.doc(), true)
        };
        if let Some(cloned) = cloned {
            let shared = if cloned.doc() == n.doc() {
                shared_doc(req, object)
            } else {
                None
            };
            payload.obj.attach(cloned, shared);
            copied = Some((n, cloned));
        }
    }
    let clone = object.clone_with(req, payload)?;
    if let Some(d) = dom_mut(req, clone) {
        d.obj.set_object(clone);
    }
    if let Some((n, cloned)) = copied
        && cloned.doc() != n.doc()
    {
        copy_document_ref(req, object, clone);
    }
    if let Some(private) = fresh
        && let Some(d) = dom_mut(req, clone)
    {
        d.obj.set_private_data(Box::new(private));
    }
    Ok(clone)
}

/// `php_dom_normalize_legacy`.
fn normalize(node: Node) {
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
                normalize(c);
                let mut attr = c.properties();
                while let Some(a) = attr {
                    normalize(a);
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
fn is_equal_node(a: Node, b: Node, spec_compliant: bool) -> bool {
    if a.kind() != b.kind() {
        return false;
    }
    match a.kind() {
        Kind::Element => {
            a.name() == b.name()
                && ns_prefix_equal(a, b)
                && ns_uri_equal(a, b)
                && unordered_equal(a.properties(), b.properties(), spec_compliant)
                && (spec_compliant || unordered_ns_equal(a.ns_def(), b.ns_def()))
                && ordered_equal(a.children(), b.children(), spec_compliant)
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
            ordered_equal(a.children(), b.children(), spec_compliant)
        }
        _ => false,
    }
}

/// `dom_get_valid_file_path`: a `file://` URI's path, or a path, made
/// absolute (`VCWD_REALPATH`, else `expand_filepath`); another scheme's
/// URI as it is; `None` where the path does not fit `MAXPATHLEN`, as both
/// of php's resolvers refuse it.
fn file_path(req: &mut Request, source: &[u8]) -> Option<Vec<u8>> {
    const MAXPATHLEN: usize = 1024;
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
    let resolved = req.realpath(local).unwrap_or_else(|| req.absolutize(local));
    let bytes = resolved.0.to_string_lossy().into_owned().into_bytes();
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
/// where the legacy ones hand it to `xmlDOMWrapAdoptNode`.
fn adopt(req: &mut Request, doc_obj: PhpObject, node: Node, doc: Node) -> bool {
    node.invalidate_document_lists();
    let old = node.doc();
    if old == Some(doc) {
        node.unlink();
    } else {
        invalidate(req, doc_obj);
        if modern::follows_spec(req, doc_obj) {
            node.unlink();
            node.set_tree_doc(Some(doc));
            if let Some(private) = dom_mut(req, doc_obj).and_then(|d| private::of(&mut d.obj)) {
                private.mapper.reconcile(Some(doc), node);
            }
        } else if !node.adopt_into(doc) {
            return false;
        }
        node.undict(old, doc);
        undict_subtree(node, old, doc);
        if let Some(shared) = crate::shared_doc(req, doc_obj) {
            let transfer = |n: Node| {
                n.transfer_document(shared);
                if n.kind() == Kind::Element {
                    let mut attr = n.properties();
                    while let Some(a) = attr {
                        a.transfer_document(shared);
                        attr = a.next();
                    }
                }
            };
            transfer(node);
            let mut at = node.children();
            while let Some(n) = at {
                transfer(n);
                at = crate::next_in_tree_order(n, node);
            }
        }
    }
    true
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
        b"Dom\\Node" | b"Dom\\Notation" => &[MODERN_NODE],
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
) -> Result<Value, Thrown> {
    let class = class(req, class_name)?;
    let object = php_ext::call::new_object(req, class)?;
    req.addref(&base.as_value());
    if let Some(d) = dom_mut(req, object) {
        d.extra = Extra::Map(Box::new(Map {
            base: Some(base),
            kind,
            dims: map::Dims::of(class_name),
        }));
    }
    Ok(object.as_value())
}

/// A list of `class_name` over the objects `array` holds, the answer of an
/// `XPath` query; `array` is the list's from here.
fn node_set(req: &mut Request, class_name: &str, array: PhpArray) -> Result<Value, Thrown> {
    let class = class(req, class_name)?;
    let object = php_ext::call::new_object(req, class)?;
    if let Some(d) = dom_mut(req, object) {
        d.extra = Extra::Map(Box::new(Map {
            base: None,
            kind: MapKind::NodeSet(array),
            dims: map::Dims::of(class_name),
        }));
    }
    Ok(object.as_value())
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

fn ordered_equal(a: Option<Node>, b: Option<Node>, spec_compliant: bool) -> bool {
    let (a, b) = (node_list(a), node_list(b));
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| is_equal_node(*x, *y, spec_compliant))
}

fn unordered_equal(a: Option<Node>, b: Option<Node>, spec_compliant: bool) -> bool {
    let (a, b) = (node_list(a), node_list(b));
    a.len() == b.len()
        && a.iter()
            .all(|x| b.iter().any(|y| is_equal_node(*x, *y, spec_compliant)))
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
    throw(
        req,
        "Error",
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
) -> Result<Value, Thrown> {
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
        throw(
            req,
            "TypeError",
            &format!("Cannot assign {given} to property {class}::${prop} of type {shown}"),
        )
    };
    match declared {
        Declared::Bool => match v.as_ref() {
            ValueRef::Bool(_) => Ok(v),
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Str(_) => {
                Ok(Value::bool(truthy(req, &v)))
            }
            _ => Err(fail(req, "bool")),
        },
        Declared::String { nullable } => match v.as_ref() {
            ValueRef::Null if nullable => Ok(Value::null()),
            ValueRef::Str(_) => {
                req.addref(&v);
                Ok(v)
            }
            ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => {
                Ok(Value::string(to_string(req, &v)?))
            }
            ValueRef::Object(o) if has_to_string(req, *o) => Ok(Value::string(to_string(req, &v)?)),
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
                req.addref(&v);
                return Ok(v);
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
    let mut at = Some(class);
    let mut depth = 0;
    while let Some(c) = at {
        let decl = req.class_decl(c)?;
        let name = decl.name.0.to_vec();
        if table(&name).is_some() {
            return Some(name);
        }
        let parent = decl.parent.clone();
        depth += 1;
        if depth > 64 {
            return None;
        }
        at = parent.and_then(|p| lookup_class(req, &p.0).ok().flatten());
    }
    None
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

/// `dom_get_doc_classmap`: the class a program registered in place of
/// `base` for the document, `base` where it registered none.
fn mapped(props: &DocProps, base: ClassId) -> ClassId {
    props
        .classmap
        .iter()
        .find(|(b, _)| *b == base)
        .map_or(base, |(_, c)| *c)
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
    Std.get_method(req, object, b"__tostring").is_some()
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
    let mut at = Some(object.class());
    while let Some(c) = at {
        let Some(decl) = req.class_decl(c) else {
            break;
        };
        if let Some(p) = decl.properties.iter().find(|p| &*p.name.0 == name) {
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
            return (decl.name.0.to_vec(), declared);
        }
        let parent = decl.parent.clone();
        at = parent.and_then(|p| lookup_class(req, &p.0).ok().flatten());
    }
    (
        class_name(req, object.class()).into_bytes(),
        Declared::String { nullable: true },
    )
}

/// `instanceof_function` over two classes: whether `class` is `of` or
/// extends it.
fn class_extends(req: &mut Request, class: ClassId, of: ClassId) -> bool {
    let mut at = Some(class);
    for _ in 0..256 {
        let Some(c) = at else { return false };
        if c == of {
            return true;
        }
        let parent = req.class_decl(c).and_then(|d| d.parent.clone());
        at = parent.and_then(|p| lookup_class(req, &p.0).ok().flatten());
    }
    false
}

/// `PHP_DOM_DEPRECATED_PROPERTY`: the deprecation, unless a dumper is
/// reading.
fn deprecated_property(req: &mut Request, message: &str) {
    if !req.state::<State>().suppress_warnings {
        deprecated(req, message);
    }
}

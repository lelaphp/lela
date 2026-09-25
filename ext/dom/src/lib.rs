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
    Dependency, DependencyKind, Extension, ExtensionBuilder, ObjectHandlers, PropertyPurpose,
    PropertyRead, PropertyTest, Std,
};
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

/// A native's entry, the shape `ExtensionBuilder` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// A property read's answer: `None` is php's `FAILURE`, a row a dumper
/// leaves out.
type Read = Result<Option<Value>, Thrown>;

/// The legacy classes as `php_dom.stub.php` declares them, in the order
/// `PHP_MINIT_FUNCTION(dom)` registers them.
const STUB: &str = r#"
final class DOMException extends Exception
{
    /**
     * Intentionally left untyped for BC reasons
     * @var int
     */
    public $code = 0;
}
interface DOMParentNode
{
    /** @param DOMNode|string $nodes */
    public function append(...$nodes): void;

    /** @param DOMNode|string $nodes */
    public function prepend(...$nodes): void;

    /** @param DOMNode|string $nodes */
    public function replaceChildren(...$nodes): void;
}
interface DOMChildNode
{
    public function remove(): void;

    /** @param DOMNode|string $nodes */
    public function before(... $nodes): void;

    /** @param DOMNode|string $nodes */
    public function after(...$nodes): void;

    /** @param DOMNode|string $nodes */
    public function replaceWith(...$nodes): void;
}
class DOMImplementation
{
    /** @tentative-return-type */
    public function hasFeature(string $feature, string $version): bool {}

    /** @return DOMDocumentType|false */
    public function createDocumentType(string $qualifiedName, string $publicId = "", string $systemId = "") {}

    /** @tentative-return-type */
    public function createDocument(?string $namespace = null, string $qualifiedName = "", ?DOMDocumentType $doctype = null): DOMDocument {}
}
class DOMNode
{
    public const int DOCUMENT_POSITION_DISCONNECTED = 0x01;
    public const int DOCUMENT_POSITION_PRECEDING = 0x02;
    public const int DOCUMENT_POSITION_FOLLOWING = 0x04;
    public const int DOCUMENT_POSITION_CONTAINS = 0x08;
    public const int DOCUMENT_POSITION_CONTAINED_BY = 0x10;
    public const int DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC = 0x20;
    public string $nodeName;
    public ?string $nodeValue;
    public int $nodeType;
    public ?DOMNode $parentNode;
    public ?DOMElement $parentElement;
    public DOMNodeList $childNodes;
    public ?DOMNode $firstChild;
    public ?DOMNode $lastChild;
    public ?DOMNode $previousSibling;
    public ?DOMNode $nextSibling;
    public ?DOMNamedNodeMap $attributes;
    public bool $isConnected;
    public ?DOMDocument $ownerDocument;
    public ?string $namespaceURI;
    public string $prefix;
    public ?string $localName;
    public ?string $baseURI;
    public string $textContent;

    /** @return DOMNode|false */
    public function appendChild(DOMNode $node) {}

    /** @tentative-return-type */
    public function C14N(bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): string|false {}

    /** @tentative-return-type */
    public function C14NFile(string $uri, bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): int|false {}

    /** @return DOMNode|false */
    public function cloneNode(bool $deep = false) {}

    /** @tentative-return-type */
    public function getLineNo(): int {}

    /** @tentative-return-type */
    public function getNodePath(): ?string {}

    /** @tentative-return-type */
    public function hasAttributes(): bool {}

    /** @tentative-return-type */
    public function hasChildNodes(): bool {}

    /** @return DOMNode|false */
    public function insertBefore(DOMNode $node, ?DOMNode $child = null) {}

    /** @tentative-return-type */
    public function isDefaultNamespace(string $namespace): bool {}

    /** @tentative-return-type */
    public function isSameNode(DOMNode $otherNode): bool {}

    public function isEqualNode(?DOMNode $otherNode): bool {}

    /** @tentative-return-type */
    public function isSupported(string $feature, string $version): bool {}

    /** @tentative-return-type */
    public function lookupNamespaceURI(?string $prefix): ?string {}

    /** @tentative-return-type */
    public function lookupPrefix(string $namespace): ?string {}

    /** @tentative-return-type */
    public function normalize(): void {}

    /** @return DOMNode|false */
    public function removeChild(DOMNode $child) {}

    /** @return DOMNode|false */
    public function replaceChild(DOMNode $node, DOMNode $child) {}

    public function contains(DOMNode|DOMNameSpaceNode|null $other): bool {}

    public function getRootNode(?array $options = null): DOMNode {}

    public function compareDocumentPosition(DOMNode $other): int {}

    public function __sleep(): array {}

    public function __wakeup(): void {}
}
class DOMNameSpaceNode
{
    public string $nodeName;
    public ?string $nodeValue;
    public int $nodeType;
    public string $prefix;
    public ?string $localName;
    public ?string $namespaceURI;
    public bool $isConnected;
    public ?DOMDocument $ownerDocument;
    public ?DOMNode $parentNode;
    public ?DOMElement $parentElement;

    /** @implementation-alias DOMNode::__sleep */
    public function __sleep(): array {}

    /** @implementation-alias DOMNode::__wakeup */
    public function __wakeup(): void {}
}
class DOMDocumentFragment extends DOMNode implements DOMParentNode
{
    public ?DOMElement $firstElementChild;
    public ?DOMElement $lastElementChild;
    public int $childElementCount;

    public function __construct() {}

    /** @tentative-return-type */
    public function appendXML(string $data): bool {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::append
     */
    public function append(...$nodes): void {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::prepend
     */
    public function prepend(...$nodes): void {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMDocument::replaceChildren
     */
    public function replaceChildren(...$nodes): void {}
}
class DOMDocument extends DOMNode implements DOMParentNode
{
    public ?DOMDocumentType $doctype;
    public DOMImplementation $implementation;
    public ?DOMElement $documentElement;

    /**
     * @readonly
     * @deprecated
     * @virtual
     */
    public ?string $actualEncoding;
    public ?string $encoding;
    public ?string $xmlEncoding;
    public bool $standalone;
    public bool $xmlStandalone;
    public ?string $version;
    public ?string $xmlVersion;
    public bool $strictErrorChecking;
    public ?string $documentURI;

    /**
     * @readonly
     * @deprecated
     * @virtual
     */
    public mixed $config;
    public bool $formatOutput;
    public bool $validateOnParse;
    public bool $resolveExternals;
    public bool $preserveWhiteSpace;
    public bool $recover;
    public bool $substituteEntities;
    public ?DOMElement $firstElementChild;
    public ?DOMElement $lastElementChild;
    public int $childElementCount;

    public function __construct(string $version = "1.0", string $encoding = "") {}

    /** @return DOMAttr|false */
    public function createAttribute(string $localName) {}

    /** @return DOMAttr|false */
    public function createAttributeNS(?string $namespace, string $qualifiedName) {}

    /** @return DOMCdataSection|false */
    public function createCDATASection(string $data) {}

    /** @tentative-return-type */
    public function createComment(string $data): DOMComment {}

    /** @tentative-return-type */
    public function createDocumentFragment(): DOMDocumentFragment {}

    /** @return DOMElement|false */
    public function createElement(string $localName, string $value = "")  {}

    /** @return DOMElement|false */
    public function createElementNS(?string $namespace, string $qualifiedName, string $value = "") {}

    /** @return DOMEntityReference|false */
    public function createEntityReference(string $name) {}

    /** @return DOMProcessingInstruction|false */
    public function createProcessingInstruction(string $target, string $data = "") {}

    /** @tentative-return-type */
    public function createTextNode(string $data): DOMText {}

    /** @tentative-return-type */
    public function getElementById(string $elementId): ?DOMElement {}

    /**
     * @tentative-return-type
     * @implementation-alias DOMElement::getElementsByTagName
     */
    public function getElementsByTagName(string $qualifiedName): DOMNodeList {}

    /**
     * @tentative-return-type
     * @implementation-alias DOMElement::getElementsByTagNameNS
     */
    public function getElementsByTagNameNS(?string $namespace, string $localName): DOMNodeList {}

    /** @return DOMNode|false */
    public function importNode(DOMNode $node, bool $deep = false) {}

    /** @tentative-return-type */
    public function load(string $filename, int $options = 0): bool {}

    /** @tentative-return-type */
    public function loadXML(string $source, int $options = 0): bool {}

    /** @tentative-return-type */
    public function normalizeDocument(): void {}

    /** @tentative-return-type */
    public function registerNodeClass(string $baseClass, ?string $extendedClass): true {}

    /** @tentative-return-type */
    public function save(string $filename, int $options = 0): int|false {}

    /** @tentative-return-type */
    public function loadHTML(string $source, int $options = 0): bool {}

    /** @tentative-return-type */
    public function loadHTMLFile(string $filename, int $options = 0): bool {}

    /** @tentative-return-type */
    public function saveHTML(?DOMNode $node = null): string|false {}

    /** @tentative-return-type */
    public function saveHTMLFile(string $filename): int|false {}

    /** @tentative-return-type */
    public function saveXML(?DOMNode $node = null, int $options = 0): string|false {}

    /** @tentative-return-type */
    public function schemaValidate(string $filename, int $flags = 0): bool {}

    /** @tentative-return-type */
    public function schemaValidateSource(string $source, int $flags = 0): bool {}

    /** @tentative-return-type */
    public function relaxNGValidate(string $filename): bool {}

    /** @tentative-return-type */
    public function relaxNGValidateSource(string $source): bool {}

    /** @tentative-return-type */
    public function validate(): bool {}

    /** @tentative-return-type */
    public function xinclude(int $options = 0): int|false {}

    /** @tentative-return-type */
    public function adoptNode(DOMNode $node): DOMNode|false {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::append
     */
    public function append(...$nodes): void {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::prepend
     */
    public function prepend(...$nodes): void {}

    /** @param DOMNode|string $nodes */
    public function replaceChildren(...$nodes): void {}
}
class DOMNodeList implements IteratorAggregate, Countable
{
    public int $length;

    /** @tentative-return-type */
    public function count(): int {}

    public function getIterator(): Iterator {}

    /** @return DOMElement|DOMNode|DOMNameSpaceNode|null */
    public function item(int $index) {}
}
class DOMNamedNodeMap implements IteratorAggregate, Countable
{
    public int $length;

    /** @tentative-return-type */
    public function getNamedItem(string $qualifiedName): ?DOMNode {}

    /** @tentative-return-type */
    public function getNamedItemNS(?string $namespace, string $localName): ?DOMNode {}

    /** @tentative-return-type */
    public function item(int $index): ?DOMNode {}

    /** @tentative-return-type */
    public function count(): int {}

    public function getIterator(): Iterator {}
}
class DOMCharacterData extends DOMNode implements DOMChildNode
{
    public string $data;
    public int $length;
    public ?DOMElement $previousElementSibling;
    public ?DOMElement $nextElementSibling;

    /** @tentative-return-type */
    public function appendData(string $data): true {}

    /** @return string|false */
    public function substringData(int $offset, int $count) {}

    /** @tentative-return-type */
    public function insertData(int $offset, string $data): bool {}

    /** @tentative-return-type */
    public function deleteData(int $offset, int $count): bool {}

    /** @tentative-return-type */
    public function replaceData(int $offset, int $count, string $data): bool {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::replaceWith
     */
    public function replaceWith(...$nodes): void {}

    /** @implementation-alias DOMElement::remove */
    public function remove(): void {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::before
     */
    public function before(... $nodes): void {}

    /**
     * @param DOMNode|string $nodes
     * @implementation-alias DOMElement::after
     */
    public function after(...$nodes): void {}
}
class DOMAttr extends DOMNode
{
    public string $name;
    public bool $specified;
    public string $value;
    public ?DOMElement $ownerElement;
    public mixed $schemaTypeInfo;

    public function __construct(string $name, string $value = "") {}

    /** @tentative-return-type */
    public function isId(): bool {}
}
class DOMElement extends DOMNode implements \DOMParentNode, \DOMChildNode
{
    public string $tagName;
    public string $className;
    public string $id;
    public mixed $schemaTypeInfo;
    public ?DOMElement $firstElementChild;
    public ?DOMElement $lastElementChild;
    public int $childElementCount;
    public ?DOMElement $previousElementSibling;
    public ?DOMElement $nextElementSibling;

    public function __construct(string $qualifiedName, ?string $value = null, string $namespace = "") {}

    /** @tentative-return-type */
    public function getAttribute(string $qualifiedName): string {}

    public function getAttributeNames(): array {}

    /** @tentative-return-type */
    public function getAttributeNS(?string $namespace, string $localName): string {}

    /** @return DOMAttr|DOMNameSpaceNode|false */
    public function getAttributeNode(string $qualifiedName) {}

    /** @return DOMAttr|DOMNameSpaceNode|null */
    public function getAttributeNodeNS(?string $namespace, string $localName) {}

    /** @tentative-return-type */
    public function getElementsByTagName(string $qualifiedName): DOMNodeList {}

    /** @tentative-return-type */
    public function getElementsByTagNameNS(?string $namespace, string $localName): DOMNodeList {}

    /** @tentative-return-type */
    public function hasAttribute(string $qualifiedName): bool {}

    /** @tentative-return-type */
    public function hasAttributeNS(?string $namespace, string $localName): bool {}

    /** @tentative-return-type */
    public function removeAttribute(string $qualifiedName): bool {}

    /** @tentative-return-type */
    public function removeAttributeNS(?string $namespace, string $localName): void {}

    /** @return DOMAttr|false */
    public function removeAttributeNode(DOMAttr $attr) {}

    /** @return DOMAttr|bool */
    public function setAttribute(string $qualifiedName, string $value) {}

    /** @tentative-return-type */
    public function setAttributeNS(?string $namespace, string $qualifiedName, string $value): void {}

    /** @return DOMAttr|null|false */
    public function setAttributeNode(DOMAttr $attr) {}

    /** @return DOMAttr|null|false */
    public function setAttributeNodeNS(DOMAttr $attr) {}

    /** @tentative-return-type */
    public function setIdAttribute(string $qualifiedName, bool $isId): void {}

    /** @tentative-return-type */
    public function setIdAttributeNS(string $namespace, string $qualifiedName, bool $isId): void {}

    /** @tentative-return-type */
    public function setIdAttributeNode(DOMAttr $attr, bool $isId): void {}

    public function toggleAttribute(string $qualifiedName, ?bool $force = null): bool {}

    public function remove(): void {}

    /** @param DOMNode|string $nodes */
    public function before(... $nodes): void {}

    /** @param DOMNode|string $nodes */
    public function after(...$nodes): void {}

    /** @param DOMNode|string $nodes */
    public function replaceWith(...$nodes): void {}

    /** @param DOMNode|string $nodes */
    public function append(...$nodes): void {}

    /** @param DOMNode|string $nodes */
    public function prepend(...$nodes): void {}

    /** @param DOMNode|string $nodes */
    public function replaceChildren(...$nodes): void {}

    public function insertAdjacentElement(string $where, DOMElement $element): ?DOMElement {}

    public function insertAdjacentText(string $where, string $data): void {}
}
class DOMText extends DOMCharacterData
{
    public string $wholeText;

    public function __construct(string $data = "") {}

    /** @tentative-return-type */
    public function isWhitespaceInElementContent(): bool {}

    /**
     * @tentative-return-type
     * @alias DOMText::isWhitespaceInElementContent
     */
    public function isElementContentWhitespace(): bool {}

    /** @return DOMText|false */
    public function splitText(int $offset) {}
}
class DOMComment extends DOMCharacterData
{
    public function __construct(string $data = "") {}
}
class DOMCdataSection extends DOMText
{
    public function __construct(string $data) {}
}
class DOMDocumentType extends DOMNode
{
    public string $name;
    public DOMNamedNodeMap $entities;
    public DOMNamedNodeMap $notations;
    public string $publicId;
    public string $systemId;
    public ?string $internalSubset;
}
class DOMNotation extends DOMNode
{
    public string $publicId;
    public string $systemId;
}
class DOMEntity extends DOMNode
{
    public ?string $publicId;
    public ?string $systemId;
    public ?string $notationName;

    /**
     * @readonly
     * @deprecated
     * @virtual
     */
    public ?string $actualEncoding;

    /**
     * @readonly
     * @deprecated
     * @virtual
     */
    public ?string $encoding;

    /**
     * @readonly
     * @deprecated
     * @virtual
     */
    public ?string $version;
}
class DOMEntityReference extends DOMNode
{
    public function __construct(string $name) {}
}
class DOMProcessingInstruction extends DOMNode
{
    public string $target;
    public string $data;

    public function __construct(string $name, string $value = "") {}
}
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
"#;

/// The `Dom\` namespace as `php_dom.stub.php` declares it (8.4), with the
/// docblocks the reader takes nothing from left out.
const MODERN_STUB: &str = r#"
namespace Dom
{
interface ParentNode
{
    public function append(Node|string ...$nodes): void;
    public function prepend(Node|string ...$nodes): void;
    public function replaceChildren(Node|string ...$nodes): void;
    public function querySelector(string $selectors): ?Element;
    public function querySelectorAll(string $selectors): NodeList;
}
interface ChildNode
{
    public function remove(): void;
    public function before(Node|string ...$nodes): void;
    public function after(Node|string ...$nodes): void;
    public function replaceWith(Node|string ...$nodes): void;
}
/** @strict-properties */
class Implementation
{
    public function createDocumentType(string $qualifiedName, string $publicId, string $systemId): DocumentType {}
    public function createDocument(?string $namespace, string $qualifiedName, ?DocumentType $doctype = null): XMLDocument {}
    public function createHTMLDocument(?string $title = null): HTMLDocument {}
}
/** @strict-properties */
class Node
{
    private final function __construct() {}
    public int $nodeType;
    public string $nodeName;
    public string $baseURI;
    public bool $isConnected;
    public ?Document $ownerDocument;
    /** @implementation-alias DOMNode::getRootNode */
    public function getRootNode(array $options = []): Node {}
    public ?Node $parentNode;
    public ?Element $parentElement;
    /** @implementation-alias DOMNode::hasChildNodes */
    public function hasChildNodes(): bool {}
    public NodeList $childNodes;
    public ?Node $firstChild;
    public ?Node $lastChild;
    public ?Node $previousSibling;
    public ?Node $nextSibling;
    public ?string $nodeValue;
    public ?string $textContent;
    /** @implementation-alias DOMNode::normalize */
    public function normalize(): void {}
    /** @implementation-alias DOMNode::cloneNode */
    public function cloneNode(bool $deep = false): Node {}
    public function isEqualNode(?Node $otherNode): bool {}
    public function isSameNode(?Node $otherNode): bool {}
    public const int DOCUMENT_POSITION_DISCONNECTED = 0x01;
    public const int DOCUMENT_POSITION_PRECEDING = 0x02;
    public const int DOCUMENT_POSITION_FOLLOWING = 0x04;
    public const int DOCUMENT_POSITION_CONTAINS = 0x08;
    public const int DOCUMENT_POSITION_CONTAINED_BY = 0x10;
    public const int DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC = 0x20;
    public function compareDocumentPosition(Node $other): int {}
    public function contains(?Node $other): bool {}
    public function lookupPrefix(?string $namespace): ?string {}
    /** @implementation-alias DOMNode::lookupNamespaceURI */
    public function lookupNamespaceURI(?string $prefix): ?string {}
    public function isDefaultNamespace(?string $namespace): bool {}
    public function insertBefore(Node $node, ?Node $child): Node {}
    public function appendChild(Node $node): Node {}
    public function replaceChild(Node $node, Node $child): Node {}
    public function removeChild(Node $child): Node {}
    /** @implementation-alias DOMNode::getLineNo */
    public function getLineNo(): int {}
    public function getNodePath(): string {}
    /** @implementation-alias DOMNode::C14N */
    public function C14N(bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): string|false {}
    /** @implementation-alias DOMNode::C14NFile */
    public function C14NFile(string $uri, bool $exclusive = false, bool $withComments = false, ?array $xpath = null, ?array $nsPrefixes = null): int|false {}
    /** @implementation-alias DOMNode::__sleep */
    public function __sleep(): array {}
    /** @implementation-alias DOMNode::__wakeup */
    public function __wakeup(): void {}
}
class NodeList implements \IteratorAggregate, \Countable
{
    public int $length;
    /** @implementation-alias DOMNodeList::count */
    public function count(): int {}
    /** @implementation-alias DOMNodeList::getIterator */
    public function getIterator(): \Iterator {}
    /** @implementation-alias DOMNodeList::item */
    public function item(int $index): ?Node {}
}
class NamedNodeMap implements \IteratorAggregate, \Countable
{
    public int $length;
    /** @implementation-alias DOMNamedNodeMap::item */
    public function item(int $index): ?Attr {}
    /** @implementation-alias DOMNamedNodeMap::getNamedItem */
    public function getNamedItem(string $qualifiedName): ?Attr {}
    /** @implementation-alias DOMNamedNodeMap::getNamedItemNS */
    public function getNamedItemNS(?string $namespace, string $localName): ?Attr {}
    /** @implementation-alias DOMNamedNodeMap::count */
    public function count(): int {}
    /** @implementation-alias DOMNamedNodeMap::getIterator */
    public function getIterator(): \Iterator {}
}
class DtdNamedNodeMap implements \IteratorAggregate, \Countable
{
    public int $length;
    /** @implementation-alias DOMNamedNodeMap::item */
    public function item(int $index): Entity|Notation|null {}
    /** @implementation-alias DOMNamedNodeMap::getNamedItem */
    public function getNamedItem(string $qualifiedName): Entity|Notation|null {}
    /** @implementation-alias DOMNamedNodeMap::getNamedItemNS */
    public function getNamedItemNS(?string $namespace, string $localName): Entity|Notation|null {}
    /** @implementation-alias DOMNamedNodeMap::count */
    public function count(): int {}
    /** @implementation-alias DOMNamedNodeMap::getIterator */
    public function getIterator(): \Iterator {}
}
class HTMLCollection implements \IteratorAggregate, \Countable
{
    public int $length;
    /** @implementation-alias DOMNodeList::item */
    public function item(int $index): ?Element {}
    public function namedItem(string $key): ?Element {}
    /** @implementation-alias DOMNodeList::count */
    public function count(): int {}
    /** @implementation-alias DOMNodeList::getIterator */
    public function getIterator(): \Iterator {}
}
enum AdjacentPosition : string
{
    case BeforeBegin = "beforebegin";
    case AfterBegin = "afterbegin";
    case BeforeEnd = "beforeend";
    case AfterEnd = "afterend";
}
class Element extends Node implements ParentNode, ChildNode
{
    public ?string $namespaceURI;
    public ?string $prefix;
    public string $localName;
    public string $tagName;
    public HTMLCollection $children;
    public ?Element $firstElementChild;
    public ?Element $lastElementChild;
    public int $childElementCount;
    public ?Element $previousElementSibling;
    public ?Element $nextElementSibling;
    public string $id;
    public string $className;
    public TokenList $classList;
    /** @implementation-alias DOMNode::hasAttributes */
    public function hasAttributes(): bool {}
    public NamedNodeMap $attributes;
    /** @implementation-alias DOMElement::getAttributeNames */
    public function getAttributeNames(): array {}
    /** @implementation-alias DOMElement::getAttribute */
    public function getAttribute(string $qualifiedName): ?string {}
    /** @implementation-alias DOMElement::getAttributeNS */
    public function getAttributeNS(?string $namespace, string $localName): ?string {}
    /** @implementation-alias DOMElement::setAttribute */
    public function setAttribute(string $qualifiedName, string $value): void {}
    /** @implementation-alias DOMElement::setAttributeNS */
    public function setAttributeNS(?string $namespace, string $qualifiedName, string $value): void {}
    public function removeAttribute(string $qualifiedName): void {}
    /** @implementation-alias DOMElement::removeAttributeNS */
    public function removeAttributeNS(?string $namespace, string $localName): void {}
    /** @implementation-alias DOMElement::toggleAttribute */
    public function toggleAttribute(string $qualifiedName, ?bool $force = null): bool {}
    /** @implementation-alias DOMElement::hasAttribute */
    public function hasAttribute(string $qualifiedName): bool {}
    /** @implementation-alias DOMElement::hasAttributeNS */
    public function hasAttributeNS(?string $namespace, string $localName): bool {}
    /** @implementation-alias DOMElement::getAttributeNode */
    public function getAttributeNode(string $qualifiedName): ?Attr {}
    /** @implementation-alias DOMElement::getAttributeNodeNS */
    public function getAttributeNodeNS(?string $namespace, string $localName): ?Attr {}
    /** @implementation-alias Dom\Element::setAttributeNodeNS */
    public function setAttributeNode(Attr $attr) : ?Attr {}
    public function setAttributeNodeNS(Attr $attr) : ?Attr {}
    public function removeAttributeNode(Attr $attr) : Attr {}
    public function getElementsByTagName(string $qualifiedName): HTMLCollection {}
    public function getElementsByTagNameNS(?string $namespace, string $localName): HTMLCollection {}
    public function getElementsByClassName(string $classNames): HTMLCollection {}
    public function insertAdjacentElement(AdjacentPosition $where, Element $element): ?Element {}
    public function insertAdjacentText(AdjacentPosition $where, string $data): void {}
    public function insertAdjacentHTML(AdjacentPosition $where, string $string): void {}
    /** @implementation-alias DOMElement::setIdAttribute */
    public function setIdAttribute(string $qualifiedName, bool $isId): void {}
    /** @implementation-alias DOMElement::setIdAttributeNS */
    public function setIdAttributeNS(?string $namespace, string $qualifiedName, bool $isId): void {}
    public function setIdAttributeNode(Attr $attr, bool $isId): void {}
    /** @implementation-alias DOMElement::remove */
    public function remove(): void {}
    /** @implementation-alias DOMElement::before */
    public function before(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::after */
    public function after(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::replaceWith */
    public function replaceWith(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::append */
    public function append(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::prepend */
    public function prepend(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::replaceChildren */
    public function replaceChildren(Node|string ...$nodes): void {}
    public function querySelector(string $selectors): ?Element {}
    public function querySelectorAll(string $selectors): NodeList {}
    public function closest(string $selectors): ?Element {}
    public function matches(string $selectors): bool {}
    public string $innerHTML;
    public string $outerHTML;
    public string $substitutedNodeValue;
    public function getInScopeNamespaces(): array {}
    public function getDescendantNamespaces(): array {}
    public function rename(?string $namespaceURI, string $qualifiedName): void {}
}
class HTMLElement extends Element
{
}
class Attr extends Node
{
    public ?string $namespaceURI;
    public ?string $prefix;
    public string $localName;
    public string $name;
    public string $value;
    public ?Element $ownerElement;
    public bool $specified;
    /** @implementation-alias DOMAttr::isId */
    public function isId(): bool {}
    /** @implementation-alias Dom\Element::rename */
    public function rename(?string $namespaceURI, string $qualifiedName): void {}
}
class CharacterData extends Node implements ChildNode
{
    public ?Element $previousElementSibling;
    public ?Element $nextElementSibling;
    public string $data;
    public int $length;
    /** @implementation-alias DOMCharacterData::substringData */
    public function substringData(int $offset, int $count): string {}
    public function appendData(string $data): void {}
    public function insertData(int $offset, string $data): void {}
    public function deleteData(int $offset, int $count): void {}
    public function replaceData(int $offset, int $count, string $data): void {}
    /** @implementation-alias DOMElement::remove */
    public function remove(): void {}
    /** @implementation-alias DOMElement::before */
    public function before(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::after */
    public function after(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::replaceWith */
    public function replaceWith(Node|string ...$nodes): void {}
}
class Text extends CharacterData
{
    /** @implementation-alias DOMText::splitText */
    public function splitText(int $offset): Text {}
    public string $wholeText;
}
class CDATASection extends Text {}
class ProcessingInstruction extends CharacterData
{
    public string $target;
}
class Comment extends CharacterData
{
}
class DocumentType extends Node implements ChildNode
{
    public string $name;
    public DtdNamedNodeMap $entities;
    public DtdNamedNodeMap $notations;
    public string $publicId;
    public string $systemId;
    public ?string $internalSubset;
    /** @implementation-alias DOMElement::remove */
    public function remove(): void {}
    /** @implementation-alias DOMElement::before */
    public function before(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::after */
    public function after(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::replaceWith */
    public function replaceWith(Node|string ...$nodes): void {}
}
class DocumentFragment extends Node implements ParentNode
{
    public HTMLCollection $children;
    public ?Element $firstElementChild;
    public ?Element $lastElementChild;
    public int $childElementCount;
    /** @implementation-alias DOMDocumentFragment::appendXML */
    public function appendXml(string $data): bool {}
    /** @implementation-alias DOMElement::append */
    public function append(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::prepend */
    public function prepend(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::replaceChildren */
    public function replaceChildren(Node|string ...$nodes): void {}
    /** @implementation-alias Dom\Element::querySelector */
    public function querySelector(string $selectors): ?Element {}
    /** @implementation-alias Dom\Element::querySelectorAll */
    public function querySelectorAll(string $selectors): NodeList {}
}
class Entity extends Node
{
    public ?string $publicId;
    public ?string $systemId;
    public ?string $notationName;
}
class EntityReference extends Node {}
class Notation extends Node
{
    public string $publicId;
    public string $systemId;
}
abstract class Document extends Node implements ParentNode
{
    public HTMLCollection $children;
    public ?Element $firstElementChild;
    public ?Element $lastElementChild;
    public int $childElementCount;
    public Implementation $implementation;
    public string $URL;
    public string $documentURI;
    public string $characterSet;
    public string $charset;
    public string $inputEncoding;
    public ?DocumentType $doctype;
    public ?Element $documentElement;
    /** @implementation-alias Dom\Element::getElementsByTagName */
    public function getElementsByTagName(string $qualifiedName): HTMLCollection {}
    /** @implementation-alias Dom\Element::getElementsByTagNameNS */
    public function getElementsByTagNameNS(?string $namespace, string $localName): HTMLCollection {}
    /** @implementation-alias Dom\Element::getElementsByClassName */
    public function getElementsByClassName(string $classNames): HTMLCollection {}
    public function createElement(string $localName): Element {}
    public function createElementNS(?string $namespace, string $qualifiedName): Element {}
    /** @implementation-alias DOMDocument::createDocumentFragment */
    public function createDocumentFragment(): DocumentFragment {}
    /** @implementation-alias DOMDocument::createTextNode */
    public function createTextNode(string $data): Text {}
    /** @implementation-alias DOMDocument::createCDATASection */
    public function createCDATASection(string $data): CDATASection {}
    /** @implementation-alias DOMDocument::createComment */
    public function createComment(string $data): Comment {}
    public function createProcessingInstruction(string $target, string $data): ProcessingInstruction {}
    public function importNode(?Node $node, bool $deep = false): Node {}
    public function adoptNode(Node $node): Node {}
    /** @implementation-alias DOMDocument::createAttribute */
    public function createAttribute(string $localName): Attr {}
    /** @implementation-alias DOMDocument::createAttributeNS */
    public function createAttributeNS(?string $namespace, string $qualifiedName): Attr {}
    /** @implementation-alias DOMDocument::getElementById */
    public function getElementById(string $elementId): ?Element {}
    public function registerNodeClass(string $baseClass, ?string $extendedClass): void {}
    /** @implementation-alias DOMDocument::schemaValidate */
    public function schemaValidate(string $filename, int $flags = 0): bool {}
    /** @implementation-alias DOMDocument::schemaValidateSource */
    public function schemaValidateSource(string $source, int $flags = 0): bool {}
    /** @implementation-alias DOMDocument::relaxNGValidate */
    public function relaxNgValidate(string $filename): bool {}
    /** @implementation-alias DOMDocument::relaxNGValidateSource */
    public function relaxNgValidateSource(string $source): bool {}
    /** @implementation-alias DOMElement::append */
    public function append(Node|string ...$nodes): void {}
    /** @implementation-alias DOMElement::prepend */
    public function prepend(Node|string ...$nodes): void {}
    /** @implementation-alias DOMDocument::replaceChildren */
    public function replaceChildren(Node|string ...$nodes): void {}
    public function importLegacyNode(\DOMNode $node, bool $deep = false): Node {}
    /** @implementation-alias Dom\Element::querySelector */
    public function querySelector(string $selectors): ?Element {}
    /** @implementation-alias Dom\Element::querySelectorAll */
    public function querySelectorAll(string $selectors): NodeList {}
    public ?HTMLElement $body;
    public ?HTMLElement $head;
    public string $title;
}
final class HTMLDocument extends Document
{
    public static function createEmpty(string $encoding = "UTF-8"): HTMLDocument {}
    public static function createFromFile(string $path, int $options = 0, ?string $overrideEncoding = null): HTMLDocument {}
    public static function createFromString(string $source, int $options = 0, ?string $overrideEncoding = null): HTMLDocument {}
    /** @implementation-alias Dom\XMLDocument::saveXml */
    public function saveXml(?Node $node = null, int $options = 0): string|false {}
    /** @implementation-alias DOMDocument::save */
    public function saveXmlFile(string $filename, int $options = 0): int|false {}
    public function saveHtml(?Node $node = null): string {}
    public function saveHtmlFile(string $filename): int|false {}
}
final class XMLDocument extends Document
{
    public static function createEmpty(string $version = "1.0", string $encoding = "UTF-8"): XMLDocument {}
    public static function createFromFile(string $path, int $options = 0, ?string $overrideEncoding = null): XMLDocument {}
    public static function createFromString(string $source, int $options = 0, ?string $overrideEncoding = null): XMLDocument {}
    public string $xmlEncoding;
    public bool $xmlStandalone;
    public string $xmlVersion;
    public bool $formatOutput;
    /** @implementation-alias DOMDocument::createEntityReference */
    public function createEntityReference(string $name): EntityReference {}
    /** @implementation-alias DOMDocument::validate */
    public function validate(): bool {}
    public function xinclude(int $options = 0): int {}
    public function saveXml(?Node $node = null, int $options = 0): string|false {}
    /** @implementation-alias DOMDocument::save */
    public function saveXmlFile(string $filename, int $options = 0): int|false {}
}
/** @strict-properties */
final class TokenList implements \IteratorAggregate, \Countable
{
    /** @implementation-alias Dom\Node::__construct */
    private function __construct() {}
    public int $length;
    public function item(int $index): ?string {}
    public function contains(string $token): bool {}
    public function add(string ...$tokens): void {}
    public function remove(string ...$tokens): void {}
    public function toggle(string $token, ?bool $force = null): bool {}
    public function replace(string $token, string $newToken): bool {}
    public function supports(string $token): bool {}
    public string $value;
    public function count(): int {}
    public function getIterator(): \Iterator {}
}
/** @strict-properties */
readonly final class NamespaceInfo
{
    public ?string $prefix;
    public ?string $namespaceURI;
    public Element $element;
    /** @implementation-alias Dom\Node::__construct */
    private function __construct() {}
}
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
"#;

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
    let entries: Vec<(&str, Entry)> = [
        node::METHODS,
        token_list::METHODS,
        document::METHODS,
        element::METHODS,
        classes::METHODS,
        map::METHODS,
        xpath::METHODS,
        modern::METHODS,
        xml_serializer::METHODS,
        html5_serializer::METHODS,
        inner_html::METHODS,
        html5_parser::METHODS,
        css::METHODS,
    ]
    .iter()
    .flat_map(|m| m.iter().copied())
    .collect();
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
    declared.sort_by_key(|(decl, _)| {
        MINIT_ORDER
            .iter()
            .position(|name| name.as_bytes() == &*decl.name.0)
            .unwrap_or(MINIT_ORDER.len())
    });
    for (decl, methods) in declared {
        let handlers: Option<&'static dyn ObjectHandlers> = match decl.name.0.as_ref() {
            b"DOMException"
            | b"DOMParentNode"
            | b"DOMChildNode"
            | b"Dom\\ParentNode"
            | b"Dom\\ChildNode"
            | b"Dom\\AdjacentPosition"
            | b"Dom\\NamespaceInfo" => None,
            b"DOMNodeList"
            | b"DOMNamedNodeMap"
            | b"Dom\\NodeList"
            | b"Dom\\NamedNodeMap"
            | b"Dom\\DtdNamedNodeMap"
            | b"Dom\\HTMLCollection" => Some(&map::HANDLERS),
            b"DOMXPath" | b"Dom\\XPath" => Some(&xpath::HANDLERS),
            b"Dom\\TokenList" => Some(&token_list::HANDLERS),
            b"Dom\\Implementation" => Some(&object::IMPLEMENTATION_HANDLERS),
            b"Dom\\Element" | b"Dom\\HTMLElement" => Some(&object::ELEMENT_HANDLERS),
            b"Dom\\DocumentFragment" | b"Dom\\Document" => Some(&object::PARENT_HANDLERS),
            _ => Some(&object::NODE_HANDLERS),
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
            value: ConstExpr::Long(value),
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

/// `dom_mark_ids_modified(obj->document)`: the modification number past
/// the minimum a fresh parse leaves, so `getElementById` stops trusting
/// libxml's ID cache.
fn mark_ids_modified(req: &mut Request, object: PhpObject) {
    if let Some(d) = dom_mut(req, object)
        && d.obj.has_document()
    {
        let at_least = 3;
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

/// `DOM_GET_THIS_OBJ`.
fn this_node(req: &mut Request) -> Result<(PhpObject, Node), Thrown> {
    let this = this(req)?;
    let node = fetch(req, this)?;
    Ok((this, node))
}

/// `DOM_GET_THIS_INTERN`: the receiver, php's "Couldn't fetch" where its
/// payload stands for nothing.
fn this_intern(req: &mut Request) -> Result<PhpObject, Thrown> {
    let this = this(req)?;
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

/// `Z_PARAM_STR_OR_NULL`: `None` for `null` or nothing given.
fn nullable_bytes_arg(req: &mut Request, position: u32) -> Result<Option<Vec<u8>>, Thrown> {
    Ok(arg(req, position)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)))
}

/// `Z_PARAM_LONG`, `default` where the call gave none.
fn long_arg(req: &mut Request, position: u32, default: i64) -> Result<i64, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_int())
        .unwrap_or(default))
}

/// `Z_PARAM_BOOL`, `default` where the call gave none.
fn bool_arg(req: &mut Request, position: u32, default: bool) -> Result<bool, Thrown> {
    Ok(arg(req, position)?
        .and_then(|v| v.as_bool())
        .unwrap_or(default))
}

/// `Z_PARAM_BOOL_OR_NULL`.
fn nullable_bool_arg(req: &mut Request, position: u32) -> Result<Option<bool>, Thrown> {
    Ok(arg(req, position)?.and_then(|v| v.as_bool()))
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

fn property_slot(req: &mut Request, object: PhpObject, name: &[u8]) -> Option<u32> {
    if handler(req, object, name).is_some() {
        return None;
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
fn file_path(req: &Request, source: &[u8]) -> Option<Vec<u8>> {
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
/// under the weak rule, over the three types dom's writable properties
/// have.
fn coerce_property(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    value: &Value,
) -> Result<Value, Thrown> {
    let declared = declared_type(req, object, name);
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
        let class = class_name(req, object.class());
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

/// The declared type of a handled property, as the stub writes it.
enum Declared {
    Bool,
    String { nullable: bool },
}

fn declared_type(req: &mut Request, object: PhpObject, name: &[u8]) -> Declared {
    let mut at = Some(object.class());
    while let Some(c) = at {
        let Some(decl) = req.class_decl(c) else {
            break;
        };
        if let Some(p) = decl.properties.iter().find(|p| &*p.name.0 == name) {
            return match &p.ty {
                Some(php_ext::decl::Type::Bool) => Declared::Bool,
                Some(php_ext::decl::Type::Nullable(_)) => Declared::String { nullable: true },
                _ => Declared::String { nullable: false },
            };
        }
        let parent = decl.parent.clone();
        at = parent.and_then(|p| lookup_class(req, &p.0).ok().flatten());
    }
    Declared::String { nullable: true }
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

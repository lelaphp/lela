//! The smaller classes: `DOMCharacterData` (`characterdata.c`),
//! `DOMText` (`text.c`), `DOMCdataSection`, `DOMComment`,
//! `DOMProcessingInstruction`, `DOMAttr`, `DOMDocumentFragment`,
//! `DOMEntityReference` and `DOMImplementation`, with what
//! `Dom\CharacterData` and `Dom\Implementation` (`domimplementation.c`)
//! do apart from them.

use php_ext::abi::{Instance, Passed, Path, Raw, This, True, Union, Untyped, Variadic};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{DocString, Kind, Name, Node, Ns, Parser, Utf8, XML_ATTRIBUTE_ID};

use crate::element::DOMElement;
use crate::modern::follows_spec;
use crate::node::DOMNode;
use crate::object::{Code, Dom};
use crate::private::{Handed, Private};
use crate::{dom_error, dom_throw, fetch, object_for, passed, returned, strict, string};

/// The content of a character node, the empty string for none:
/// `php_dom_get_content_or_empty`.
fn content(node: Node) -> Vec<u8> {
    node.content().unwrap_or_default()
}

/// `INDEX_SIZE_ERR` as the document's strictness says, `false` answered.
fn index_error(req: &mut Request, this: PhpObject) -> Result<Value, Thrown> {
    let strict = strict(req, this);
    dom_error(req, Code::IndexSize, strict)?;
    Ok(Value::bool(false))
}

/// `ZEND_LONG_INT_OVFL`: past what php's `int` holds.
fn overflows(n: i64) -> bool {
    n > i64::from(i32::MAX)
}

/// C's `(unsigned int)` of a number: its low 32 bits.
fn as_uint(n: i64) -> u32 {
    u32::try_from(n & 0xFFFF_FFFF).unwrap_or(u32::MAX)
}

/// `dom_convert_number_unsigned`: a negative number wraps round where the
/// document follows the standard and is an index error otherwise, `None`
/// once raised.
fn to_unsigned(req: &mut Request, this: PhpObject, input: i64) -> Result<Option<u32>, Thrown> {
    if input < 0 && !follows_spec(req, this) {
        let strict = strict(req, this);
        dom_error(req, Code::IndexSize, strict)?;
        return Ok(None);
    }
    Ok(Some(as_uint(input)))
}

/// What a data method answers, php's `return_true`: `true` from the
/// legacy classes, nothing from the `Dom\` ones, whose methods are
/// `void`; a document hands out one kind or the other, so it says which.
fn done(modern: bool) -> Value {
    if modern {
        Value::null()
    } else {
        Value::bool(true)
    }
}

/// `dom_character_data_substring_data`'s body.
fn substring_data(
    req: &mut Request,
    this: PhpObject,
    offset: i64,
    count: i64,
) -> Result<Owner<Value>, Thrown> {
    let node = fetch(req, this)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if overflows(offset) || overflows(count) {
        return index_error(req, this).map(|v| Owner::take(req, v));
    }
    let Some(offset) = to_unsigned(req, this, offset)? else {
        return Ok(Owner::bool(false));
    };
    let Some(count) = to_unsigned(req, this, count)? else {
        return Ok(Owner::bool(false));
    };
    if offset > as_uint(length) {
        return index_error(req, this).map(|v| Owner::take(req, v));
    }
    let count = count.min(as_uint(length).wrapping_sub(offset));
    let sub = Utf8::sub(&cur, i64::from(offset), i64::from(count)).unwrap_or_default();
    string(req, &sub)
}

/// `dom_character_data_insert_data`.
fn insert_data(
    req: &mut Request,
    this: PhpObject,
    offset: i64,
    data: &[u8],
) -> Result<Value, Thrown> {
    let node = fetch(req, this)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if overflows(offset) {
        return index_error(req, this);
    }
    let Some(offset) = to_unsigned(req, this, offset)? else {
        return Ok(Value::bool(false));
    };
    if offset > as_uint(length) {
        return index_error(req, this);
    }
    let offset = i64::from(offset);
    let first = Utf8::ndup(&cur, offset).unwrap_or_default();
    let second = Utf8::sub(&cur, offset, length - offset).unwrap_or_default();
    node.set_content(&first);
    node.add_content(data);
    node.add_content(&second);
    Ok(done(follows_spec(req, this)))
}

/// `dom_character_data_delete_data`.
fn delete_data(
    req: &mut Request,
    this: PhpObject,
    offset: i64,
    count: i64,
) -> Result<Value, Thrown> {
    let node = fetch(req, this)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if offset < 0 || overflows(offset) || overflows(count) || offset > length {
        return index_error(req, this);
    }
    let Some(count) = to_unsigned(req, this, count)? else {
        return Ok(Value::bool(false));
    };
    let mut out = if offset > 0 {
        Utf8::sub(&cur, 0, offset).unwrap_or_default()
    } else {
        Vec::new()
    };
    let count = i64::from(count).min(length - offset);
    out.extend(Utf8::sub(&cur, offset + count, length - offset).unwrap_or_default());
    node.set_content(&out);
    Ok(done(follows_spec(req, this)))
}

/// `dom_character_data_replace_data`: `xmlStrcat`'s copy of the data
/// stops at a NUL byte.
fn replace_data(
    req: &mut Request,
    this: PhpObject,
    (offset, count): (i64, i64),
    data: &[u8],
) -> Result<Value, Thrown> {
    let node = fetch(req, this)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if offset < 0 || overflows(offset) || overflows(count) || offset > length {
        return index_error(req, this);
    }
    let Some(count) = to_unsigned(req, this, count)? else {
        return Ok(Value::bool(false));
    };
    let mut out = if offset > 0 {
        Utf8::sub(&cur, 0, offset).unwrap_or_default()
    } else {
        Vec::new()
    };
    let count = i64::from(count).min(length - offset);
    let data = php_ext::coerce::c_str(data);
    out.extend_from_slice(data);
    if offset < length {
        out.extend(Utf8::sub(&cur, offset + count, length - offset).unwrap_or_default());
    }
    node.set_content(&out);
    Ok(done(follows_spec(req, this)))
}

/// A node laid down by a constructor, the receiver rebound to it, or
/// php's invalid state where libxml made none.
fn construct(req: &mut Request, this: PhpObject, node: Option<Node>) -> Result<(), Thrown> {
    match node {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `DOMImplementation::hasFeature(string $feature, string $version): bool`.
///
/// ```php
/// $i = new DOMImplementation;
/// var_dump($i->hasFeature('XML', '1.0'), $i->hasFeature('Events', '2.0'));
/// $t = $i->createDocumentType('r', 'p', 's');
/// $d = $i->createDocument('urn:r', 'x:r', $t);
/// echo $d->saveXML();
/// var_dump($t->ownerDocument === $d);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// <?xml version="1.0"?>
/// <!DOCTYPE r PUBLIC "p" "s">
/// <x:r xmlns:x="urn:r"/>
/// bool(true)
/// ```
#[php_class(size = 80, payload = Dom, handlers = crate::object::NODE_HANDLERS)]
pub(crate) struct DOMImplementation;

#[php_class]
impl DOMImplementation {
    /// `DOMImplementation::hasFeature(string $feature, string $version): bool`.
    #[php(tentative)]
    fn hasFeature(_this: This<Self>, feature: PhpString, version: PhpString) -> bool {
        crate::has_feature(feature.as_bytes(), version.as_bytes())
    }

    /// `DOMImplementation::createDocumentType(string $qualifiedName, string $publicId = "", string $systemId = "")`.
    fn createDocumentType(
        req: &mut Request,
        _this: This<Self>,
        qualifiedName: PhpString,
        #[php(default = "")] publicId: PhpString,
        #[php(default = "")] systemId: PhpString,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let name = qualifiedName.as_bytes();
        if name.is_empty() {
            return Err(arg_value_error(req, 1, "not be empty"));
        }
        if name.windows(3).any(|w| w == b"%00") {
            php_ext::raising::docref(
                req,
                php_ext::raising::Level::Warning,
                "URI must not contain percent-encoded NUL bytes",
            );
            return Ok(Untyped(Owner::bool(false)));
        }
        let non_empty = |v: &[u8]| (!v.is_empty()).then(|| v.to_vec());
        let dtd = Node::create_int_subset(
            None,
            name,
            non_empty(publicId.as_bytes()).as_deref(),
            non_empty(systemId.as_bytes()).as_deref(),
        );
        match dtd {
            Some(dtd) => object_for(req, dtd, None).map(Untyped),
            None => Err(dom_throw(req, Code::InvalidState)),
        }
    }

    /// `DOMImplementation::createDocument(?string $namespace = null, string $qualifiedName = "", ?DOMDocumentType $doctype = null)`.
    #[php(tentative)]
    fn createDocument(
        req: &mut Request,
        _this: This<Self>,
        #[php(default = null)] namespace: Option<PhpString>,
        #[php(default = "")] qualifiedName: PhpString,
        #[php(default = null)] doctype: Option<Instance<DOMDocumentType>>,
    ) -> Result<Owner<Raw<Instance<crate::document::DOMDocument>>>, Thrown> {
        let uri = namespace.as_ref().map_or(&[][..], PhpString::as_bytes);
        let name = qualifiedName.as_bytes();
        let doctype = match doctype {
            Some(o) => {
                let dtd = fetch(req, o.object())?;
                if dtd.kind() == Kind::DocumentType {
                    return Err(arg_value_error(req, 3, "is an invalid DocumentType object"));
                }
                if dtd.doc().is_some() {
                    return Err(dom_throw(req, Code::WrongDocument));
                }
                Some(dtd)
            }
            None => None,
        };
        let mut parts = None;
        if !name.is_empty() {
            match crate::check_qname(name, 1) {
                Ok((local, prefix)) => {
                    let ns = if uri.is_empty() {
                        None
                    } else {
                        match Ns::new(None, Some(uri), prefix.as_deref()) {
                            Some(ns) => Some(ns),
                            None => return Err(dom_throw(req, Code::Namespace)),
                        }
                    };
                    parts = Some((local, ns));
                }
                Err(code) => return Err(dom_throw(req, code)),
            }
        }
        let Some(doc) = Node::new_doc(None) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        if let Some(dtd) = doctype {
            doc.set_int_subset(Some(dtd));
            dtd.set_parent(Some(doc));
            dtd.set_tree_doc(Some(doc));
            doc.set_children(Some(dtd));
            doc.set_last(Some(dtd));
        }
        if let Some((local, ns)) = parts {
            let Some(root) = Node::new_element(Some(doc), ns, &local, None) else {
                return Err(dom_throw(req, Code::InvalidState));
            };
            root.set_ns_def(ns);
            doc.set_root(root);
        }
        let value = object_for(req, doc, None)?;
        if let (Some(dtd), Some(object)) = (doctype, value.as_object().copied())
            && let Some(shared) = crate::shared_doc(req, object)
        {
            dtd.share_document(req, shared);
        }
        returned(value)
    }
}

/// `DOMDocumentFragment::appendXML(string $data): bool`.
///
/// ```php
/// $d = new DOMDocument;
/// $d->loadXML('<r/>');
/// $f = $d->createDocumentFragment();
/// var_dump($f->appendXML('<a/>t<b>u</b>'));
/// $d->documentElement->appendChild($f);
/// $root = $d->documentElement;
/// echo $d->saveXML($root), "\n";
/// try { (new DOMDocumentFragment)->appendXML('<a/>'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// <r><a/>t<b>u</b></r>
/// No Modification Allowed Error
/// ```
#[php_class(extends(DOMNode), implements(crate::parent::DOMParentNode))]
pub(crate) struct DOMDocumentFragment;

#[php_class]
impl DOMDocumentFragment {
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const childElementCount: i64;

    /// `DOMDocumentFragment::__construct()`.
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        construct(req, this.object(), Node::new_fragment(None))
    }

    /// `DOMDocumentFragment::appendXML(string $data): bool`.
    #[php(tentative)]
    fn appendXML(req: &mut Request, this: This<Self>, data: PhpString) -> Result<bool, Thrown> {
        let this = this.object();
        let node = fetch(req, this)?;
        if crate::is_read_only(node) {
            let strict = strict(req, this);
            dom_error(req, Code::NoModificationAllowed, strict)?;
            return Ok(false);
        }
        let (parsed, raised) = Parser::chunk(node.doc(), data.as_bytes());
        Errors::route(req, raised);
        match parsed {
            Err(()) => Ok(false),
            Ok(list) => {
                if let Some(first) = list {
                    node.add_child_list(first);
                }
                Ok(true)
            }
        }
    }

    #[php(implementation = DOMElement::append)]
    fn append(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::prepend)]
    fn prepend(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::replaceChildren)]
    fn replaceChildren(this: This<Self>, nodes: Variadic<Untyped<Value>>);
}

/// `class DOMCharacterData extends DOMNode implements DOMChildNode`: its
/// four properties `@virtual`, the data methods, and `DOMElement`'s child
/// methods as its own (`@implementation-alias`).
///
/// ```php
/// $c = new DOMComment('a');
/// var_dump($c->appendData('b'), $c->data, $c->insertData(1, 'X'), $c->data, $c->deleteData(0, 1), $c->data, $c->replaceData(1, 1, 'YZ'), $c->data);
/// ```
/// ```expect
/// bool(true)
/// string(2) "ab"
/// bool(true)
/// string(3) "aXb"
/// bool(true)
/// string(2) "Xb"
/// bool(true)
/// string(3) "XYZ"
/// ```
#[php_class(extends(DOMNode), implements(crate::parent::DOMChildNode))]
pub(crate) struct DOMCharacterData;

#[php_class]
impl DOMCharacterData {
    #[php(property, virtual)]
    const data: PhpString;
    #[php(property, virtual)]
    const length: i64;
    #[php(property, virtual)]
    const previousElementSibling: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const nextElementSibling: Option<Instance<DOMElement>>;

    /// `DOMCharacterData::appendData(string $data): true`.
    #[php(tentative)]
    fn appendData(req: &mut Request, this: This<Self>, data: PhpString) -> Result<True, Thrown> {
        fetch(req, this.object())?.text_concat(data.as_bytes());
        Ok(True)
    }

    /// `DOMCharacterData::substringData(int $offset, int $count)`, and
    /// `Dom\CharacterData`'s, where a negative number wraps round.
    ///
    /// ```php
    /// $t = new DOMText('héllo');
    /// var_dump($t->substringData(1, 3), $t->substringData(3, 99));
    /// $d = new DOMDocument; $t = $d->createTextNode('abc');
    /// try { $t->substringData(5, 1); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $t->substringData(-1, 1); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// $m = Dom\XMLDocument::createEmpty();
    /// $t = $m->createTextNode('abcdef');
    /// var_dump($t->substringData(1, -1), $t->substringData(-(2**32 - 2), 2));
    /// try { $t->substringData(-1, 1); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(4) "éll"
    /// string(2) "lo"
    /// Index Size Error
    /// Index Size Error
    /// string(5) "bcdef"
    /// string(2) "cd"
    /// Index Size Error
    /// ```
    fn substringData(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
        count: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        substring_data(req, this.object(), offset, count).map(Untyped)
    }

    /// `DOMCharacterData::insertData(int $offset, string $data): bool`,
    /// which `Dom\CharacterData` shares: `true` from the legacy classes,
    /// nothing from the `Dom\` ones, whose methods are `void`.
    #[php(tentative)]
    fn insertData(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
        data: PhpString,
    ) -> Result<Owner<Raw<bool>>, Thrown> {
        let done = insert_data(req, this.object(), offset, data.as_bytes())?;
        returned(Owner::take(req, done))
    }

    /// `DOMCharacterData::deleteData(int $offset, int $count): bool`.
    #[php(tentative)]
    fn deleteData(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
        count: i64,
    ) -> Result<Owner<Raw<bool>>, Thrown> {
        let done = delete_data(req, this.object(), offset, count)?;
        returned(Owner::take(req, done))
    }

    /// `DOMCharacterData::replaceData(int $offset, int $count, string $data): bool`.
    #[php(tentative)]
    fn replaceData(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
        count: i64,
        data: PhpString,
    ) -> Result<Owner<Raw<bool>>, Thrown> {
        let done = replace_data(req, this.object(), (offset, count), data.as_bytes())?;
        returned(Owner::take(req, done))
    }

    #[php(implementation = DOMElement::replaceWith)]
    fn replaceWith(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::remove)]
    fn remove(this: This<Self>);
    #[php(implementation = DOMElement::before)]
    fn before(this: This<Self>, nodes: Variadic<Untyped<Value>>);
    #[php(implementation = DOMElement::after)]
    fn after(this: This<Self>, nodes: Variadic<Untyped<Value>>);
}

/// `DOMAttr::__construct(string $name, string $value = "")`.
///
/// ```php
/// $a = new DOMAttr('a', 'v');
/// var_dump($a->name, $a->value, $a->isId(), $a->ownerElement);
/// ```
/// ```expect
/// string(1) "a"
/// string(1) "v"
/// bool(false)
/// NULL
/// ```
#[php_class(extends(DOMNode))]
pub(crate) struct DOMAttr;

#[php_class]
impl DOMAttr {
    #[php(property, virtual)]
    const name: PhpString;
    #[php(property, virtual)]
    const specified: bool;
    #[php(property, virtual)]
    const value: PhpString;
    #[php(property, virtual)]
    const ownerElement: Option<Instance<DOMElement>>;
    #[php(property, virtual)]
    const schemaTypeInfo: Value;

    /// `DOMAttr::__construct(string $name, string $value = "")`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        #[php(default = "")] value: Passed<PhpString>,
    ) -> Result<(), Thrown> {
        let name = name.as_bytes();
        let value = passed(value);
        if !Name::valid(name) {
            return Err(dom_throw(req, Code::InvalidCharacter));
        }
        construct(req, this.object(), Node::new_prop(name, value.as_deref()))
    }

    /// `DOMAttr::isId(): bool`.
    #[php(tentative)]
    fn isId(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(fetch(req, this.object())?.atype() == XML_ATTRIBUTE_ID)
    }
}

/// `DOMText::__construct(string $data = "")`.
///
/// ```php
/// $t = new DOMText('ab');
/// var_dump($t->data, (new DOMText)->data, (new DOMCdataSection('c'))->data, (new DOMComment)->data, (new DOMEntityReference('e'))->nodeName);
/// ```
/// ```expect
/// string(2) "ab"
/// string(0) ""
/// string(1) "c"
/// string(0) ""
/// string(1) "e"
/// ```
#[php_class(extends(DOMCharacterData))]
pub(crate) struct DOMText;

#[php_class]
impl DOMText {
    #[php(property, virtual)]
    const wholeText: PhpString;

    /// `DOMText::__construct(string $data = "")`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] data: Passed<PhpString>,
    ) -> Result<(), Thrown> {
        let data = passed(data);
        construct(req, this.object(), Node::new_text_detached(data.as_deref()))
    }

    /// `DOMText::isWhitespaceInElementContent(): bool`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML("<r> <a/>x</r>");
    /// var_dump($d->documentElement->firstChild->isWhitespaceInElementContent(), $d->documentElement->lastChild->isElementContentWhitespace());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn isWhitespaceInElementContent(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(fetch(req, this.object())?.is_blank())
    }

    /// `@alias DOMText::isWhitespaceInElementContent`.
    #[php(tentative, implementation = DOMText::isWhitespaceInElementContent)]
    fn isElementContentWhitespace(this: This<Self>) -> bool;

    /// `DOMText::splitText(int $offset)`, and `Dom\Text::splitText`.
    ///
    /// ```php
    /// $d = new DOMDocument;
    /// $d->loadXML('<r>hello</r>');
    /// $t = $d->documentElement->firstChild;
    /// $n = $t->splitText(2);
    /// var_dump($t->data, $n->data, $d->documentElement->childNodes->length, $t->splitText(99));
    /// try { $t->splitText(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(2) "he"
    /// string(3) "llo"
    /// int(2)
    /// bool(false)
    /// DOMText::splitText(): Argument #1 ($offset) must be greater than or equal to 0
    /// ```
    ///
    /// `Dom\Text::splitText` throws where the offset is past the end:
    ///
    /// ```php
    /// $d = Dom\XMLDocument::createFromString('<r>hello</r>');
    /// $t = $d->documentElement->firstChild;
    /// $n = $t->splitText(5);
    /// var_dump(get_class($n), $n->data, $d->documentElement->childNodes->length);
    /// try { $t->splitText(6); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(8) "Dom\Text"
    /// string(0) ""
    /// int(2)
    /// Index Size Error
    /// ```
    fn splitText(
        req: &mut Request,
        this: This<Self>,
        offset: i64,
    ) -> Result<Untyped<Owner<Value>>, Thrown> {
        let this = this.object();
        let node = fetch(req, this)?;
        if offset < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        let cur = content(node);
        let length = Utf8::len(&cur);
        if overflows(offset) || offset > length {
            if follows_spec(req, this) {
                return Err(dom_throw(req, Code::IndexSize));
            }
            return Ok(Untyped(Owner::bool(false)));
        }
        let first = Utf8::ndup(&cur, offset).unwrap_or_default();
        let second = Utf8::sub(&cur, offset, length - offset).unwrap_or_default();
        node.set_content(&first);
        let Some(split) = Node::new_text(node.doc(), &second) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        if node.parent().is_some() {
            split.set_kind(Kind::Element);
            node.add_next_sibling(split);
            split.set_kind(Kind::Text);
        }
        object_for(req, split, Some(this)).map(Untyped)
    }
}

/// `class DOMComment extends DOMCharacterData`.
#[php_class(extends(DOMCharacterData))]
pub(crate) struct DOMComment;

#[php_class]
impl DOMComment {
    /// `DOMComment::__construct(string $data = "")`.
    ///
    /// ```php
    /// var_dump((new DOMComment('c'))->data, (new DOMComment)->data);
    /// ```
    /// ```expect
    /// string(1) "c"
    /// string(0) ""
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = "")] data: Passed<PhpString>,
    ) -> Result<(), Thrown> {
        let data = passed(data);
        construct(
            req,
            this.object(),
            Node::new_comment_detached(data.as_deref()),
        )
    }
}

/// `class DOMCdataSection extends DOMText`.
#[php_class(extends(DOMText))]
pub(crate) struct DOMCdataSection;

#[php_class]
impl DOMCdataSection {
    /// `DOMCdataSection::__construct(string $data)`.
    ///
    /// ```php
    /// $c = new DOMCdataSection('c');
    /// var_dump($c->nodeName, $c->data);
    /// ```
    /// ```expect
    /// string(14) "#cdata-section"
    /// string(1) "c"
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, data: PhpString) -> Result<(), Thrown> {
        construct(req, this.object(), Node::new_cdata(None, data.as_bytes()))
    }
}

/// `class DOMDocumentType extends DOMNode`, its six properties
/// `@virtual`.
#[php_class(extends(DOMNode))]
pub(crate) struct DOMDocumentType;

#[php_class]
impl DOMDocumentType {
    #[php(property, virtual)]
    const name: PhpString;
    #[php(property, virtual)]
    const entities: Instance<crate::map::DOMNamedNodeMap>;
    #[php(property, virtual)]
    const notations: Instance<crate::map::DOMNamedNodeMap>;
    #[php(property, virtual)]
    const publicId: PhpString;
    #[php(property, virtual)]
    const systemId: PhpString;
    #[php(property, virtual)]
    const internalSubset: Option<PhpString>;
}

/// `class DOMNotation extends DOMNode`.
#[php_class(extends(DOMNode))]
pub(crate) struct DOMNotation;

#[php_class]
impl DOMNotation {
    #[php(property, virtual)]
    const publicId: PhpString;
    #[php(property, virtual)]
    const systemId: PhpString;
}

/// `class DOMEntity extends DOMNode`, whose last three properties php
/// deprecates.
#[php_class(extends(DOMNode))]
pub(crate) struct DOMEntity;

#[php_class]
impl DOMEntity {
    #[php(property, virtual)]
    const publicId: Option<PhpString>;
    #[php(property, virtual)]
    const systemId: Option<PhpString>;
    #[php(property, virtual)]
    const notationName: Option<PhpString>;
    #[php(property, virtual)]
    const actualEncoding: Option<PhpString>;
    #[php(property, virtual)]
    const encoding: Option<PhpString>;
    #[php(property, virtual)]
    const version: Option<PhpString>;
}

/// `class DOMEntityReference extends DOMNode`.
#[php_class(extends(DOMNode))]
pub(crate) struct DOMEntityReference;

#[php_class]
impl DOMEntityReference {
    /// `DOMEntityReference::__construct(string $name)`.
    ///
    /// ```php
    /// var_dump((new DOMEntityReference('e'))->nodeName);
    /// try { new DOMEntityReference('1'); } catch (DOMException $x) { echo $x->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "e"
    /// Invalid Character Error
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, name: PhpString) -> Result<(), Thrown> {
        let name = name.as_bytes();
        if !Name::valid(name) {
            return Err(dom_throw(req, Code::InvalidCharacter));
        }
        construct(req, this.object(), Node::new_reference(None, name))
    }
}

/// `DOMProcessingInstruction::__construct(string $name, string $value = "")`.
///
/// ```php
/// $p = new DOMProcessingInstruction('t', 'd');
/// var_dump($p->target, $p->data);
/// try { new DOMProcessingInstruction('1'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "t"
/// string(1) "d"
/// Invalid Character Error
/// ```
#[php_class(extends(DOMNode))]
pub(crate) struct DOMProcessingInstruction;

#[php_class]
impl DOMProcessingInstruction {
    #[php(property, virtual)]
    const target: PhpString;
    #[php(property, virtual)]
    const data: PhpString;

    /// `DOMProcessingInstruction::__construct(string $name, string $value = "")`.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        name: PhpString,
        #[php(default = "")] value: Passed<PhpString>,
    ) -> Result<(), Thrown> {
        let name = name.as_bytes();
        let value = passed(value);
        if !Name::valid(name) {
            return Err(dom_throw(req, Code::InvalidCharacter));
        }
        construct(
            req,
            this.object(),
            Node::new_pi_detached(name, value.as_deref()),
        )
    }
}

/// `class Dom\Implementation`, `@strict-properties` and
/// `@not-serializable`: one per document, never cloned
/// ([`crate::object::IMPLEMENTATION_HANDLERS`]).
#[php_class(
    namespace(Dom),
    not_serializable,
    dynamic_properties = forbidden,
    size = 80, payload = Dom,
    handlers = crate::object::IMPLEMENTATION_HANDLERS
)]
pub(crate) struct Implementation;

#[php_class]
impl Implementation {
    /// `Dom\Implementation::createDocumentType(string $qualifiedName, string $publicId, string $systemId): Dom\DocumentType`.
    ///
    /// ```php
    /// $i = Dom\XMLDocument::createEmpty()->implementation;
    /// $t = $i->createDocumentType('p:r', '', 's');
    /// var_dump(get_class($t), $t->name, $t->publicId, $t->systemId, $t->ownerDocument);
    /// try { $i->createDocumentType('1r', '', ''); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $i->createDocumentType("r\0", '', ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(16) "Dom\DocumentType"
    /// string(3) "p:r"
    /// string(0) ""
    /// string(1) "s"
    /// NULL
    /// Namespace Error
    /// Dom\Implementation::createDocumentType(): Argument #1 ($qualifiedName) must not contain any null bytes
    /// ```
    fn createDocumentType(
        req: &mut Request,
        _this: This<Self>,
        qualifiedName: Path,
        publicId: Path,
        systemId: Path,
    ) -> Result<Owner<Raw<Instance<DocumentType>>>, Thrown> {
        let name = qualifiedName.0.as_bytes();
        if !Name::valid_qname(name) {
            return Err(dom_throw(req, Code::Namespace));
        }
        let non_empty = |v: &[u8]| (!v.is_empty()).then(|| v.to_vec());
        let Some(dtd) = Node::create_int_subset(
            None,
            name,
            non_empty(publicId.0.as_bytes()).as_deref(),
            non_empty(systemId.0.as_bytes()).as_deref(),
        ) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        let class = crate::class(req, "Dom\\DocumentType")?;
        let made = crate::instantiate(req, class, dtd, None)?;
        returned(made)
    }

    /// `Dom\Implementation::createDocument(?string $namespace, string $qualifiedName, ?Dom\DocumentType $doctype = null): Dom\XMLDocument`.
    ///
    /// ```php
    /// $i = Dom\XMLDocument::createEmpty()->implementation;
    /// $t = $i->createDocumentType('r', '', '');
    /// $d = $i->createDocument('urn:r', 'x:r', $t);
    /// $r = $d->documentElement;
    /// var_dump(get_class($d), $d->xmlVersion, $d->xmlEncoding, $d->doctype === $t, $t->ownerDocument === $d, $r->nodeName, $r->namespaceURI, $r->attributes->length, $d->childNodes->length);
    /// var_dump($i->createDocument(null, '')->childNodes->length);
    /// try { $i->createDocument(null, 'x:r'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// try { $i->createDocument(null, '1'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(15) "Dom\XMLDocument"
    /// string(3) "1.0"
    /// string(5) "UTF-8"
    /// bool(true)
    /// bool(true)
    /// string(3) "x:r"
    /// string(5) "urn:r"
    /// int(0)
    /// int(2)
    /// int(0)
    /// Namespace Error
    /// Invalid Character Error
    /// ```
    fn createDocument(
        req: &mut Request,
        _this: This<Self>,
        namespace: Option<Path>,
        qualifiedName: Path,
        #[php(default = null)] doctype: Option<Instance<DocumentType>>,
    ) -> Result<Owner<Raw<Instance<crate::document::XMLDocument>>>, Thrown> {
        let uri = namespace.as_ref().map(|n| n.0.as_bytes());
        let qualified_name = qualifiedName.0.as_bytes();
        let doctype = match doctype {
            Some(o) => Some(fetch(req, o.object())?),
            None => None,
        };
        let Some(document) = Node::new_doc(Some(b"1.0")) else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        let mut private = Private::new(req)?;
        document.set_doc_string(DocString::Encoding, Some(b"UTF-8"));
        let mut element = None;
        if !qualified_name.is_empty() {
            match crate::validate_and_extract(uri, qualified_name) {
                Ok((local, prefix)) => {
                    let ns = private.mapper.get_raw_prefix(
                        req,
                        Some(document),
                        prefix.as_deref().unwrap_or_default(),
                        Handed::given(namespace.as_ref().map(|n| n.0)),
                    );
                    let ns = match ns {
                        Ok(ns) => ns,
                        Err(thrown) => {
                            document.free_document();
                            private.free(req);
                            return Err(thrown);
                        }
                    };
                    let Some(e) = Node::new_element(Some(document), ns, &local, None) else {
                        document.free_document();
                        private.free(req);
                        return Err(dom_throw(req, Code::InvalidState));
                    };
                    element = Some(e);
                }
                Err(code) => {
                    document.free_document();
                    private.free(req);
                    return Err(dom_throw(req, code));
                }
            }
        }
        let object = crate::modern::document_object(req, "Dom\\XMLDocument", document, private)?;
        if let Some(dtd) = doctype {
            if let Err(thrown) = crate::adopt(req, *object, dtd, document) {
                if let Some(e) = element {
                    e.free();
                }
                object.give_up(req);
                return Err(thrown);
            }
            document.add_child(dtd);
            dtd.set_tree_doc(Some(document));
            document.set_int_subset(Some(dtd));
        }
        if let Some(e) = element {
            document.add_child(e);
        }
        returned(object.into_value())
    }

    /// `Dom\Implementation::createHTMLDocument(?string $title = null): Dom\HTMLDocument`:
    /// the `html`, `head`, optional `title` and `body` elements in the HTML
    /// namespace under an `html` doctype.
    ///
    /// ```php
    /// $i = Dom\XMLDocument::createEmpty()->implementation;
    /// $d = $i->createHTMLDocument('T&amp;');
    /// $html = $d->documentElement;
    /// var_dump(get_class($d), $d->doctype->name, $html->namespaceURI, $html->childNodes->length, $html->firstChild->firstChild->textContent, $html->lastChild->nodeName, $d->characterSet);
    /// var_dump($i->createHTMLDocument()->documentElement->firstChild->hasChildNodes());
    /// ```
    /// ```expect
    /// string(16) "Dom\HTMLDocument"
    /// string(4) "html"
    /// string(28) "http://www.w3.org/1999/xhtml"
    /// int(2)
    /// string(6) "T&amp;"
    /// string(4) "BODY"
    /// string(5) "UTF-8"
    /// bool(false)
    /// ```
    fn createHTMLDocument(
        req: &mut Request,
        _this: This<Self>,
        #[php(default = null)] title: Option<Path>,
    ) -> Result<Owner<Raw<Instance<crate::html_document::HTMLDocument>>>, Thrown> {
        let title = title.map(|t| t.0.as_bytes().to_vec());
        let Some(doc) = Node::new_html_doc() else {
            return Err(dom_throw(req, Code::InvalidState));
        };
        doc.set_doc_string(DocString::Encoding, Some(b"UTF-8"));
        let dtd = Node::create_int_subset(Some(doc), b"html", None, None);
        let mut private = Private::new(req)?;
        let html_ns = match private.mapper.html(req, Some(doc)) {
            Ok(ns) => ns,
            Err(thrown) => {
                doc.free_document();
                private.free(req);
                return Err(thrown);
            }
        };
        let element = |name: &[u8], parent: Node| {
            let e = Node::new_element(Some(doc), html_ns, name, None)?;
            parent.add_child(e);
            Some(e)
        };
        let html = element(b"html", doc);
        let head = html.and_then(|h| element(b"head", h));
        let title_ok = match (&title, head) {
            (None, _) => true,
            (Some(t), Some(head)) => element(b"title", head)
                .and_then(|e| Node::new_text(Some(doc), t).map(|text| e.add_child(text)))
                .is_some(),
            (Some(_), None) => false,
        };
        let body = html.and_then(|h| element(b"body", h));
        if dtd.is_none() || head.is_none() || !title_ok || body.is_none() {
            doc.free_document();
            private.free(req);
            return Err(dom_throw(req, Code::InvalidState));
        }
        let object = crate::modern::document_object(req, "Dom\\HTMLDocument", doc, private)?;
        returned(object.into_value())
    }
}

/// `class Dom\CharacterData extends Dom\Node implements Dom\ChildNode`:
/// `DOMCharacterData`'s data methods, answering nothing.
///
/// ```php
/// $d = Dom\XMLDocument::createEmpty();
/// $c = $d->createComment('foobarbaz');
/// var_dump($c->appendData('!'), $c->data);
/// var_dump($c->insertData(1, 'X'), $c->data, $c->deleteData(0, 1), $c->data, $c->replaceData(1, 1, 'YZ'), $c->data);
/// $c->deleteData(3, -1);
/// var_dump($c->data);
/// $c->insertData(-(2**32 - 1), 'A');
/// var_dump($c->data);
/// $c->replaceData(1, -1, 'B');
/// var_dump($c->data);
/// try { $c->insertData(-1, 'A'); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// try { $c->deleteData(-1, 1); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// string(10) "foobarbaz!"
/// NULL
/// string(11) "fXoobarbaz!"
/// NULL
/// string(10) "Xoobarbaz!"
/// NULL
/// string(11) "XYZobarbaz!"
/// string(3) "XYZ"
/// string(4) "XAYZ"
/// string(2) "XB"
/// Index Size Error
/// Index Size Error
/// ```
#[php_class(
    namespace(Dom),
    extends(crate::modern::Node),
    implements(crate::parent::ChildNode)
)]
pub(crate) struct CharacterData;

#[php_class]
impl CharacterData {
    #[php(property, virtual)]
    const previousElementSibling: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const nextElementSibling: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const data: PhpString;
    #[php(property, virtual)]
    const length: i64;

    #[php(implementation = DOMCharacterData::substringData)]
    fn substringData(this: This<Self>, offset: i64, count: i64) -> PhpString;

    /// `Dom\CharacterData::appendData(string $data): void`.
    fn appendData(req: &mut Request, this: This<Self>, data: PhpString) -> Result<(), Thrown> {
        fetch(req, this.object())?.text_concat(data.as_bytes());
        Ok(())
    }

    #[php(implementation = DOMCharacterData::insertData)]
    fn insertData(this: This<Self>, offset: i64, data: PhpString);
    #[php(implementation = DOMCharacterData::deleteData)]
    fn deleteData(this: This<Self>, offset: i64, count: i64);
    #[php(implementation = DOMCharacterData::replaceData)]
    fn replaceData(this: This<Self>, offset: i64, count: i64, data: PhpString);
    #[php(implementation = DOMElement::remove)]
    fn remove(this: This<Self>);
    #[php(implementation = DOMElement::before)]
    fn before(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::after)]
    fn after(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceWith)]
    fn replaceWith(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
}

/// `class Dom\Text extends Dom\CharacterData`.
#[php_class(namespace(Dom), extends(CharacterData))]
pub(crate) struct Text;

#[php_class]
impl Text {
    #[php(property, virtual)]
    const wholeText: PhpString;
    #[php(implementation = DOMText::splitText)]
    fn splitText(this: This<Self>, offset: i64) -> Instance<Text>;
}

/// `class Dom\CDATASection extends Dom\Text`.
#[php_class(namespace(Dom), extends(Text))]
pub(crate) struct CDATASection;

#[php_class]
impl CDATASection {}

/// `class Dom\ProcessingInstruction extends Dom\CharacterData`.
#[php_class(namespace(Dom), extends(CharacterData))]
pub(crate) struct ProcessingInstruction;

#[php_class]
impl ProcessingInstruction {
    #[php(property, virtual)]
    const target: PhpString;
}

/// `class Dom\Comment extends Dom\CharacterData`.
#[php_class(namespace(Dom), extends(CharacterData))]
pub(crate) struct Comment;

#[php_class]
impl Comment {}

/// `class Dom\DocumentType extends Dom\Node implements Dom\ChildNode`.
#[php_class(
    namespace(Dom),
    extends(crate::modern::Node),
    implements(crate::parent::ChildNode)
)]
pub(crate) struct DocumentType;

#[php_class]
impl DocumentType {
    #[php(property, virtual)]
    const name: PhpString;
    #[php(property, virtual)]
    const entities: Instance<crate::map::DtdNamedNodeMap>;
    #[php(property, virtual)]
    const notations: Instance<crate::map::DtdNamedNodeMap>;
    #[php(property, virtual)]
    const publicId: PhpString;
    #[php(property, virtual)]
    const systemId: PhpString;
    #[php(property, virtual)]
    const internalSubset: Option<PhpString>;
    #[php(implementation = DOMElement::remove)]
    fn remove(this: This<Self>);
    #[php(implementation = DOMElement::before)]
    fn before(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::after)]
    fn after(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceWith)]
    fn replaceWith(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
}

/// `class Dom\DocumentFragment extends Dom\Node implements
/// Dom\ParentNode`, whose clone drops the cached `$children`
/// ([`crate::object::PARENT_HANDLERS`]).
#[php_class(
    namespace(Dom),
    extends(crate::modern::Node),
    implements(crate::parent::ParentNode),
    handlers = crate::object::PARENT_HANDLERS
)]
pub(crate) struct DocumentFragment;

#[php_class]
impl DocumentFragment {
    #[php(property)]
    const children: Instance<crate::map::HTMLCollection>;
    #[php(property, virtual)]
    const firstElementChild: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const lastElementChild: Option<Instance<crate::element::Element>>;
    #[php(property, virtual)]
    const childElementCount: i64;
    #[php(implementation = DOMDocumentFragment::appendXML)]
    fn appendXml(this: This<Self>, data: PhpString) -> bool;
    #[php(implementation = DOMElement::append)]
    fn append(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::prepend)]
    fn prepend(this: This<Self>, nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>);
    #[php(implementation = DOMElement::replaceChildren)]
    fn replaceChildren(
        this: This<Self>,
        nodes: Variadic<Union<Instance<crate::modern::Node>, PhpString>>,
    );
    #[php(implementation = crate::element::Element::querySelector)]
    fn querySelector(
        this: This<Self>,
        selectors: PhpString,
    ) -> Option<Instance<crate::element::Element>>;
    #[php(implementation = crate::element::Element::querySelectorAll)]
    fn querySelectorAll(this: This<Self>, selectors: PhpString) -> Instance<crate::map::NodeList>;
}

/// `class Dom\Entity extends Dom\Node`.
#[php_class(namespace(Dom), extends(crate::modern::Node))]
pub(crate) struct Entity;

#[php_class]
impl Entity {
    #[php(property, virtual)]
    const publicId: Option<PhpString>;
    #[php(property, virtual)]
    const systemId: Option<PhpString>;
    #[php(property, virtual)]
    const notationName: Option<PhpString>;
}

/// `class Dom\EntityReference extends Dom\Node`.
#[php_class(namespace(Dom), extends(crate::modern::Node))]
pub(crate) struct EntityReference;

#[php_class]
impl EntityReference {}

/// `class Dom\Notation extends Dom\Node`.
#[php_class(namespace(Dom), extends(crate::modern::Node))]
pub(crate) struct Notation;

#[php_class]
impl Notation {
    #[php(property, virtual)]
    const publicId: PhpString;
    #[php(property, virtual)]
    const systemId: PhpString;
}

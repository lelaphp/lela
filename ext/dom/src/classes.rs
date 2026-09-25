//! The smaller legacy classes: `DOMCharacterData` (`characterdata.c`),
//! `DOMText` (`text.c`), `DOMCdataSection`, `DOMComment`,
//! `DOMProcessingInstruction`, `DOMAttr`, `DOMDocumentFragment`,
//! `DOMEntityReference` and `DOMImplementation`.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Kind, Name, Node, Ns, Parser, Utf8, XML_ATTRIBUTE_ID};

use crate::object::Code;
use crate::{
    Entry, arity, bytes_arg, dom_error, dom_throw, fetch, long_arg, nullable_bytes_arg, object_arg,
    object_for, strict, string, this, this_node,
};

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

/// Whether an offset fits php's `int`, `ZEND_LONG_INT_OVFL`'s test.
fn fits(n: i64) -> bool {
    i32::try_from(n).is_ok()
}

/// `DOMCharacterData::substringData(int $offset, int $count)`.
///
/// ```php
/// $t = new DOMText('héllo');
/// var_dump($t->substringData(1, 3), $t->substringData(3, 99));
/// $d = new DOMDocument; $t = $d->createTextNode('abc');
/// try { $t->substringData(5, 1); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "éll"
/// string(2) "lo"
/// Index Size Error
/// ```
fn substring_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let (this, node) = this_node(req)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if !fits(offset) || !fits(count) || offset < 0 || count < 0 || offset > length {
        return index_error(req, this);
    }
    let count = count.min(length - offset);
    string(req, &Utf8::sub(&cur, offset, count).unwrap_or_default())
}

/// `DOMCharacterData::appendData(string $data): true`.
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
fn append_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (_, node) = this_node(req)?;
    node.text_concat(&data);
    Ok(Value::bool(true))
}

/// `DOMCharacterData::insertData(int $offset, string $data): bool`.
fn insert_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let data = bytes_arg(req, 2)?;
    let (this, node) = this_node(req)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if !fits(offset) || offset < 0 || offset > length {
        return index_error(req, this);
    }
    let first = Utf8::ndup(&cur, offset).unwrap_or_default();
    let second = Utf8::sub(&cur, offset, length - offset).unwrap_or_default();
    node.set_content(&first);
    node.add_content(&data);
    node.add_content(&second);
    Ok(Value::bool(true))
}

/// `DOMCharacterData::deleteData(int $offset, int $count): bool`.
fn delete_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let (this, node) = this_node(req)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if offset < 0 || !fits(offset) || !fits(count) || offset > length || count < 0 {
        return index_error(req, this);
    }
    let mut out = if offset > 0 {
        Utf8::sub(&cur, 0, offset).unwrap_or_default()
    } else {
        Vec::new()
    };
    let count = count.min(length - offset);
    out.extend(Utf8::sub(&cur, offset + count, length - offset).unwrap_or_default());
    node.set_content(&out);
    Ok(Value::bool(true))
}

/// `DOMCharacterData::replaceData(int $offset, int $count, string $data): bool`.
fn replace_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let data = bytes_arg(req, 3)?;
    let (this, node) = this_node(req)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if offset < 0 || !fits(offset) || !fits(count) || offset > length || count < 0 {
        return index_error(req, this);
    }
    let mut out = if offset > 0 {
        Utf8::sub(&cur, 0, offset).unwrap_or_default()
    } else {
        Vec::new()
    };
    let count = count.min(length - offset);
    out.extend_from_slice(&data);
    if offset < length {
        out.extend(Utf8::sub(&cur, offset + count, length - offset).unwrap_or_default());
    }
    node.set_content(&out);
    Ok(Value::bool(true))
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
fn text_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let data = crate::arg(req, 1)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let this = this(req)?;
    match Node::new_text_detached(data.as_deref()) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `DOMText::splitText(int $offset)`.
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
fn split_text(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let offset = long_arg(req, 1, 0)?;
    let (this, node) = this_node(req)?;
    if offset < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    let cur = content(node);
    let length = Utf8::len(&cur);
    if !fits(offset) || offset > length {
        return Ok(Value::bool(false));
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
    object_for(req, split, Some(this))
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
fn is_whitespace(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(node.is_blank()))
}

/// `DOMCdataSection::__construct(string $data)`.
fn cdata_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let this = this(req)?;
    match Node::new_cdata(None, &data) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `DOMComment::__construct(string $data = "")`.
fn comment_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let data = crate::arg(req, 1)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let this = this(req)?;
    match Node::new_comment_detached(data.as_deref()) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
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
fn pi_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let name = bytes_arg(req, 1)?;
    let value = crate::arg(req, 2)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let this = this(req)?;
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    match Node::new_pi_detached(&name, value.as_deref()) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
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
fn attr_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 2)?;
    let name = bytes_arg(req, 1)?;
    let value = crate::arg(req, 2)?.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    let this = this(req)?;
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    match Node::new_prop(&name, value.as_deref()) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `DOMAttr::isId(): bool`.
fn is_id(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let (_, node) = this_node(req)?;
    Ok(Value::bool(node.atype() == XML_ATTRIBUTE_ID))
}

/// `DOMDocumentFragment::__construct()`.
fn fragment_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let this = this(req)?;
    match Node::new_fragment(None) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
        }
        None => Err(dom_throw(req, Code::InvalidState)),
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
fn append_xml(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (this, node) = this_node(req)?;
    if crate::is_read_only(node) {
        let strict = strict(req, this);
        dom_error(req, Code::NoModificationAllowed, strict)?;
        return Ok(Value::bool(false));
    }
    let (parsed, raised) = Parser::chunk(node.doc(), &data);
    Errors::route(req, raised);
    match parsed {
        Err(()) => Ok(Value::bool(false)),
        Ok(list) => {
            if let Some(first) = list {
                node.add_child_list(first);
            }
            Ok(Value::bool(true))
        }
    }
}

/// `DOMEntityReference::__construct(string $name)`.
fn entity_reference_construct(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let name = bytes_arg(req, 1)?;
    let this = this(req)?;
    if !Name::valid(&name) {
        return Err(dom_throw(req, Code::InvalidCharacter));
    }
    match Node::new_reference(None, &name) {
        Some(node) => {
            crate::rebind(req, this, node);
            Ok(Value::null())
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
fn has_feature(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let feature = bytes_arg(req, 1)?;
    let version = bytes_arg(req, 2)?;
    Ok(Value::bool(crate::has_feature(&feature, &version)))
}

/// `DOMImplementation::createDocumentType(string $qualifiedName, string $publicId = "", string $systemId = "")`.
fn create_document_type(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 3)?;
    let name = bytes_arg(req, 1)?;
    let public_id = bytes_arg(req, 2)?;
    let system_id = bytes_arg(req, 3)?;
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if name.windows(3).any(|w| w == b"%00") {
        php_ext::raising::docref(
            req,
            php_ext::raising::Level::Warning,
            "URI must not contain percent-encoded NUL bytes",
        );
        return Ok(Value::bool(false));
    }
    let non_empty = |v: &[u8]| (!v.is_empty()).then(|| v.to_vec());
    let dtd = Node::create_int_subset(
        None,
        &name,
        non_empty(&public_id).as_deref(),
        non_empty(&system_id).as_deref(),
    );
    match dtd {
        Some(dtd) => object_for(req, dtd, None),
        None => Err(dom_throw(req, Code::InvalidState)),
    }
}

/// `DOMImplementation::createDocument(?string $namespace = null, string $qualifiedName = "", ?DOMDocumentType $doctype = null)`.
fn create_document(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 3)?;
    let uri = nullable_bytes_arg(req, 1)?.unwrap_or_default();
    let name = bytes_arg(req, 2)?;
    let doctype_obj = object_arg(req, 3)?;
    let doctype = match doctype_obj {
        Some(o) => {
            let dtd = fetch(req, o)?;
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
        match crate::check_qname(&name, 1) {
            Ok((local, prefix)) => {
                let ns = if uri.is_empty() {
                    None
                } else {
                    match Ns::new(None, Some(&uri), prefix.as_deref()) {
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
        dtd.share_document(shared);
    }
    Ok(value)
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("DOMCharacterData::substringData", substring_data),
    ("DOMCharacterData::appendData", append_data),
    ("DOMCharacterData::insertData", insert_data),
    ("DOMCharacterData::deleteData", delete_data),
    ("DOMCharacterData::replaceData", replace_data),
    ("DOMCharacterData::replaceWith", crate::parent::REPLACE_WITH),
    ("DOMCharacterData::remove", crate::parent::REMOVE),
    ("DOMCharacterData::before", crate::parent::BEFORE),
    ("DOMCharacterData::after", crate::parent::AFTER),
    ("DOMText::__construct", text_construct),
    ("DOMText::isWhitespaceInElementContent", is_whitespace),
    ("DOMText::isElementContentWhitespace", is_whitespace),
    ("DOMText::splitText", split_text),
    ("DOMCdataSection::__construct", cdata_construct),
    ("DOMComment::__construct", comment_construct),
    ("DOMProcessingInstruction::__construct", pi_construct),
    ("DOMAttr::__construct", attr_construct),
    ("DOMAttr::isId", is_id),
    ("DOMDocumentFragment::__construct", fragment_construct),
    ("DOMDocumentFragment::appendXML", append_xml),
    ("DOMDocumentFragment::append", crate::parent::APPEND),
    ("DOMDocumentFragment::prepend", crate::parent::PREPEND),
    (
        "DOMDocumentFragment::replaceChildren",
        crate::parent::REPLACE_CHILDREN,
    ),
    (
        "DOMEntityReference::__construct",
        entity_reference_construct,
    ),
    ("DOMImplementation::hasFeature", has_feature),
    (
        "DOMImplementation::createDocumentType",
        create_document_type,
    ),
    ("DOMImplementation::createDocument", create_document),
];

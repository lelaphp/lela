//! The smaller classes: `DOMCharacterData` (`characterdata.c`),
//! `DOMText` (`text.c`), `DOMCdataSection`, `DOMComment`,
//! `DOMProcessingInstruction`, `DOMAttr`, `DOMDocumentFragment`,
//! `DOMEntityReference` and `DOMImplementation`, with what
//! `Dom\CharacterData` and `Dom\Implementation` (`domimplementation.c`)
//! do apart from them.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{PhpObject, Value};
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{DocString, Kind, Name, Node, Ns, Parser, Utf8, XML_ATTRIBUTE_ID};

use crate::modern::follows_spec;
use crate::object::Code;
use crate::private::Private;
use crate::{
    Entry, arity, bytes_arg, dom_error, dom_throw, fetch, long_arg, nullable_bytes_arg, object_arg,
    object_for, path_arg, strict, string, this, this_node,
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
fn substring_data(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let (this, node) = this_node(req)?;
    let cur = content(node);
    let length = Utf8::len(&cur);
    if overflows(offset) || overflows(count) {
        return index_error(req, this);
    }
    let Some(offset) = to_unsigned(req, this, offset)? else {
        return Ok(Value::bool(false));
    };
    let Some(count) = to_unsigned(req, this, count)? else {
        return Ok(Value::bool(false));
    };
    if offset > as_uint(length) {
        return index_error(req, this);
    }
    let count = count.min(as_uint(length).wrapping_sub(offset));
    let sub = Utf8::sub(&cur, i64::from(offset), i64::from(count)).unwrap_or_default();
    string(req, &sub)
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
    append_data_common(req)?;
    Ok(Value::bool(true))
}

/// `dom_character_data_append_data`.
fn append_data_common(req: &mut Request) -> Result<(), Thrown> {
    arity(req, 1, 1)?;
    let data = bytes_arg(req, 1)?;
    let (_, node) = this_node(req)?;
    node.text_concat(&data);
    Ok(())
}

/// `Dom\CharacterData::appendData(string $data): void`, and the other data
/// methods of `Dom\CharacterData`, which answer nothing and take a
/// negative count or offset round modulo 2³².
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
fn modern_append_data(req: &mut Request) -> Result<Value, Thrown> {
    append_data_common(req)?;
    Ok(Value::null())
}

/// `dom_character_data_insert_data`.
fn insert_data_common(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let data = bytes_arg(req, 2)?;
    let (this, node) = this_node(req)?;
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
    node.add_content(&data);
    node.add_content(&second);
    Ok(done(follows_spec(req, this)))
}

/// `DOMCharacterData::insertData(int $offset, string $data): bool`.
fn insert_data(req: &mut Request) -> Result<Value, Thrown> {
    insert_data_common(req)
}

/// `dom_character_data_delete_data`.
fn delete_data_common(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 2)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let (this, node) = this_node(req)?;
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

/// `DOMCharacterData::deleteData(int $offset, int $count): bool`.
fn delete_data(req: &mut Request) -> Result<Value, Thrown> {
    delete_data_common(req)
}

/// `dom_character_data_replace_data`: `xmlStrcat`'s copy of the data
/// stops at a NUL byte.
fn replace_data_common(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let offset = long_arg(req, 1, 0)?;
    let count = long_arg(req, 2, 0)?;
    let data = bytes_arg(req, 3)?;
    let (this, node) = this_node(req)?;
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
    let data = &data[..data.iter().position(|b| *b == 0).unwrap_or(data.len())];
    out.extend_from_slice(data);
    if offset < length {
        out.extend(Utf8::sub(&cur, offset + count, length - offset).unwrap_or_default());
    }
    node.set_content(&out);
    Ok(done(follows_spec(req, this)))
}

/// `DOMCharacterData::replaceData(int $offset, int $count, string $data): bool`.
fn replace_data(req: &mut Request) -> Result<Value, Thrown> {
    replace_data_common(req)
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
fn split_text(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let offset = long_arg(req, 1, 0)?;
    let (this, node) = this_node(req)?;
    if offset < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    let cur = content(node);
    let length = Utf8::len(&cur);
    if overflows(offset) || offset > length {
        if follows_spec(req, this) {
            return Err(dom_throw(req, Code::IndexSize));
        }
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

/// `Z_PARAM_PATH_OR_NULL`: the bytes, `None` for null, php's
/// `ValueError` where they hold a NUL.
fn nullable_path_arg(req: &mut Request, position: u32) -> Result<Option<Vec<u8>>, Thrown> {
    match nullable_bytes_arg(req, position)? {
        Some(bytes) if bytes.contains(&0) => {
            Err(arg_value_error(req, position, "not contain any null bytes"))
        }
        bytes => Ok(bytes),
    }
}

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
fn modern_create_document_type(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 3, 3)?;
    let name = path_arg(req, 1)?;
    let public_id = path_arg(req, 2)?;
    let system_id = path_arg(req, 3)?;
    if !Name::valid_qname(&name) {
        return Err(dom_throw(req, Code::Namespace));
    }
    let non_empty = |v: &[u8]| (!v.is_empty()).then(|| v.to_vec());
    let Some(dtd) = Node::create_int_subset(
        None,
        &name,
        non_empty(&public_id).as_deref(),
        non_empty(&system_id).as_deref(),
    ) else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    let class = crate::class(req, "Dom\\DocumentType")?;
    crate::instantiate(req, class, dtd, None)
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
fn modern_create_document(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 2, 3)?;
    let uri = nullable_path_arg(req, 1)?;
    let qualified_name = path_arg(req, 2)?;
    let doctype = match object_arg(req, 3)? {
        Some(o) => Some(fetch(req, o)?),
        None => None,
    };
    let mut private = Private::default();
    let Some(document) = Node::new_doc(Some(b"1.0")) else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    document.set_doc_string(DocString::Encoding, Some(b"UTF-8"));
    let mut element = None;
    if !qualified_name.is_empty() {
        match crate::validate_and_extract(uri.as_deref(), &qualified_name) {
            Ok((local, prefix)) => {
                let ns = private.mapper.get(
                    Some(document),
                    prefix.as_deref().unwrap_or_default(),
                    uri.as_deref().unwrap_or_default(),
                );
                let Some(e) = Node::new_element(Some(document), ns, &local, None) else {
                    document.free_document();
                    return Err(dom_throw(req, Code::InvalidState));
                };
                element = Some(e);
            }
            Err(code) => {
                document.free_document();
                return Err(dom_throw(req, code));
            }
        }
    }
    let object = crate::modern::document_object(req, "Dom\\XMLDocument", document)?;
    if let Some(d) = crate::dom_mut(req, object) {
        d.obj.set_private_data(Box::new(private));
    }
    if let Some(dtd) = doctype {
        crate::adopt(req, object, dtd, document);
        document.add_child(dtd);
        dtd.set_tree_doc(Some(document));
        document.set_int_subset(Some(dtd));
    }
    if let Some(e) = element {
        document.add_child(e);
    }
    Ok(object.as_value())
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
fn modern_create_html_document(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let title = nullable_path_arg(req, 1)?;
    let Some(doc) = Node::new_html_doc() else {
        return Err(dom_throw(req, Code::InvalidState));
    };
    doc.set_doc_string(DocString::Encoding, Some(b"UTF-8"));
    let dtd = Node::create_int_subset(Some(doc), b"html", None, None);
    let mut private = Private::default();
    let html_ns = private.mapper.html(Some(doc));
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
        return Err(dom_throw(req, Code::InvalidState));
    }
    let object = crate::modern::document_object(req, "Dom\\HTMLDocument", doc)?;
    if let Some(d) = crate::dom_mut(req, object) {
        d.obj.set_private_data(Box::new(private));
    }
    Ok(object.as_value())
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
    ("Dom\\CharacterData::appendData", modern_append_data),
    ("Dom\\CharacterData::insertData", insert_data),
    ("Dom\\CharacterData::deleteData", delete_data),
    ("Dom\\CharacterData::replaceData", replace_data),
    (
        "Dom\\Implementation::createDocumentType",
        modern_create_document_type,
    ),
    (
        "Dom\\Implementation::createDocument",
        modern_create_document,
    ),
    (
        "Dom\\Implementation::createHTMLDocument",
        modern_create_html_document,
    ),
];

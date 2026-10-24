//! `php_encoding.c`: a PHP value to an XML node and back, by the encoder
//! a WSDL type, a typemap or the value's own type names. One serialisation
//! or deserialisation keeps its references in a [`Cx`], the caller's, since
//! a [`Node`] is no `Send` value and so no part of [`State`].

// A line-by-line port of php's C: its names (`enc`, `encode`, `ns`, `nsp`),
// its argument lists and its `switch` arms are kept as php has them.
#![allow(
    clippy::similar_names,
    clippy::many_single_char_names,
    clippy::single_match_else,
    clippy::too_many_arguments
)]

mod array;
mod object;
mod scalar;
mod time;

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use php_ext::call::lookup_class;
use php_ext::request::Request;
use php_ext::values::{ClassId, Owner, PhpObject, PhpRef, PhpString, Value, ValueRef, walk};
use php_ext_libxml::ffi::{Kind, Library, Node, Ns};

use crate::model::{
    self, EncRef, Encoder, SOAP_1_1_ENC_NAMESPACE, SOAP_1_2_ENC_NAMESPACE, Sdl, ToXml, ToZval,
    Type, TypeKind, Use, XSI_NAMESPACE, ty,
};
use crate::sdl::{get_encoder, get_encoder_ex, get_encoder_from_prefix};
use crate::shared::{VAR_ENC_NAME, VAR_ENC_NAMENS, VAR_ENC_NS, VAR_ENC_STYPE, VAR_ENC_TYPE};
use crate::table::Key as TKey;
use crate::xml::{attr_content, encode_special_chars, get_attribute, href, parse_namespace};
use crate::{Error, Res, SOAP_1_1, SOAP_1_2, State, soap_error};

/// What one serialisation or deserialisation keeps: `SOAP_GLOBAL(ref_map)`.
#[derive(Default)]
pub(crate) struct Cx {
    /// Whether `ref_map` is set: off outside `encode_reset_ns` and
    /// `encode_finish`, and while an attribute or a list is written.
    tracking: bool,
    /// The values written so far, by identity, with the node each became.
    sent: HashMap<Ident, Node>,
    /// The value each node was read as, one owner each, this table's.
    read: HashMap<Node, Owner<Value>>,
    /// The PHP reference the value being written was reached through,
    /// php's `zval *` for an array behind one.
    slot: Option<PhpRef>,
    /// How many values `State::held` had when this serialisation began.
    held_mark: usize,
}

/// What `soap_check_zval_ref` keys a written value by: an object, or the
/// PHP reference an array sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Ident {
    Object(u32),
    Ref(PhpRef),
}

impl Cx {
    fn clear(&mut self, req: &mut Request) {
        self.sent.clear();
        self.slot = None;
        for (_, v) in self.read.drain() {
            v.give_up(req);
        }
    }
}

/// `encode_reset_ns`: the numbers reset and a fresh `ref_map`.
pub(crate) fn encode_reset_ns(req: &mut Request, cx: &mut Cx) {
    let st = req.state::<State>();
    st.cur_uniq_ns = 0;
    st.cur_uniq_ref = 0;
    cx.clear(req);
    cx.tracking = true;
    cx.held_mark = req.state::<State>().held.len();
}

/// `encode_finish`: the numbers reset and the `ref_map` dropped.
pub(crate) fn encode_finish(req: &mut Request, cx: &mut Cx) {
    let st = req.state::<State>();
    st.cur_uniq_ns = 0;
    st.cur_uniq_ref = 0;
    cx.clear(req);
    cx.tracking = false;
    let held = &mut req.state::<State>().held;
    let mark = cx.held_mark.min(held.len());
    let done: Vec<Owner<Value>> = held.drain(mark..).collect();
    for v in done {
        v.give_up(req);
    }
}

// ---- encoders

/// An `encodePtr` with the `encode` it names.
#[derive(Debug, Clone)]
pub(super) struct Enc {
    pub(super) r: EncRef,
    pub(super) e: Encoder,
}

/// The `encode` an `encodePtr` names in the call's description and typemap.
pub(super) fn enc(req: &mut Request, r: EncRef) -> Res<Enc> {
    let st = req.state::<State>();
    match crate::sdl::encoder(st.sdl.as_deref(), st.typemap.as_ref(), r) {
        Some(e) => Ok(Enc { r, e }),
        None => Err(soap_error("Encoding: Cannot find encoding")),
    }
}

/// `get_conversion`.
pub(super) fn get_conversion(type_: i32) -> Res<EncRef> {
    model::get_conversion(type_).ok_or_else(|| soap_error("Encoding: Cannot find encoding"))
}

/// `get_conversion`, the encoder read.
pub(super) fn conv(req: &mut Request, type_: i32) -> Res<Enc> {
    let r = get_conversion(type_)?;
    enc(req, r)
}

/// `SOAP_GLOBAL(sdl)`.
pub(super) fn sdl(req: &mut Request) -> Option<Arc<Sdl>> {
    req.state::<State>().sdl.clone()
}

/// `SOAP_GLOBAL(soap_version)`.
pub(super) fn version(req: &mut Request) -> i64 {
    req.state::<State>().soap_version
}

/// `SOAP_GLOBAL(features)`.
pub(super) fn features(req: &mut Request) -> i64 {
    req.state::<State>().features
}

/// The `sdlType` an encoder points at.
pub(super) fn sdl_type<'a>(sdl: Option<&'a Arc<Sdl>>, e: &Encoder) -> Option<&'a Type> {
    sdl?.ty(e.details.sdl_type?)
}

/// The kind of an encoder's `sdlType` and that type's own encoder.
fn chain(
    req: &mut Request,
    sdl: Option<&Arc<Sdl>>,
    r: EncRef,
) -> Res<Option<(TypeKind, Option<EncRef>)>> {
    let e = enc(req, r)?;
    Ok(sdl_type(sdl, &e.e).map(|t| (t.kind, t.encode)))
}

/// The typemap's encoder named `ns:type`, `zend_hash_find_ptr(typemap, nscat)`.
fn typemap_find(req: &mut Request, key: &[u8]) -> Option<EncRef> {
    let tm = req.state::<State>().typemap.as_ref()?;
    let at = tm
        .iter()
        .position(|(k, _)| matches!(k, TKey::Str(s) if s.as_slice() == key))?;
    u32::try_from(at).ok().map(EncRef::Typemap)
}

/// `ns:type`, or `type` where there is no namespace, as `smart_str` builds it.
fn ns_type(ns: Option<&[u8]>, type_: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(ns) = ns {
        out.extend_from_slice(ns);
        out.push(b':');
    }
    out.extend_from_slice(type_);
    out
}

/// The typemap's replacement for an encoder named by its `type_str`.
fn typemap_for(req: &mut Request, e: &Encoder) -> Option<EncRef> {
    req.state::<State>().typemap.as_ref()?;
    let t = e.details.type_str.as_deref()?;
    typemap_find(req, &ns_type(e.details.ns.as_deref(), t))
}

// ---- small pieces

/// Bytes for a message, as `%s` prints them.
pub(super) fn text(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// `xmlNewDocNode(doc, NULL, name, NULL)`.
pub(super) fn new_node(doc: Option<Node>, name: &[u8]) -> Res<Node> {
    Node::new_element(doc, None, name, None).ok_or_else(|| soap_error("Encoding: Internal Error"))
}

/// `xmlNewDocNode(parent->doc, NULL, "BOGUS", NULL)` and `xmlAddChild(parent, ret)`.
pub(super) fn bogus(parent: Node) -> Res<Node> {
    let node = new_node(parent.doc(), b"BOGUS")?;
    parent.add_child(node);
    Ok(node)
}

/// `!zval || Z_TYPE_P(zval) == IS_NULL`.
pub(super) fn is_null(v: &Value) -> bool {
    matches!(v.as_ref(), ValueRef::Undef | ValueRef::Null)
}

/// `FIND_ZVAL_NULL`: an empty value's node marked nil where encoded.
pub(super) fn find_zval_null(req: &mut Request, data: &Value, node: Node, style: Use) -> bool {
    if is_null(data) {
        if style == Use::Encoded {
            set_xsi_nil(req, node);
        }
        return true;
    }
    false
}

/// `FIND_XML_NULL`: whether the node carries `xsi:nil`.
pub(super) fn xml_nil(node: Node) -> bool {
    node.properties()
        .is_some_and(|p| get_attribute(Some(p), b"nil", Some(XSI_NAMESPACE)).is_some())
}

/// `Z_TYPE_P`, the zval's type byte.
pub(super) fn type_code(v: &Value) -> i32 {
    match v.as_ref() {
        ValueRef::Undef => 0,
        ValueRef::Null => ty::IS_NULL,
        ValueRef::Bool(false) => ty::IS_FALSE,
        ValueRef::Bool(true) => ty::IS_TRUE,
        ValueRef::Int(_) => ty::IS_LONG,
        ValueRef::Float(_) => ty::IS_DOUBLE,
        ValueRef::Str(_) => ty::IS_STRING,
        ValueRef::Array(_) => ty::IS_ARRAY,
        ValueRef::Object(_) => ty::IS_OBJECT,
        ValueRef::Resource(_) => 9,
        ValueRef::Reference(_) => 10,
    }
}

/// A string value, made in the request's heap, one owner.
pub(super) fn string(req: &mut Request, b: &[u8]) -> Res<Owner<Value>> {
    Ok(PhpString::new(req, b)?.into_value())
}

/// The class named, linked by now.
pub(super) fn class(req: &mut Request, name: &str) -> Res<ClassId> {
    match lookup_class(req, name.as_bytes())? {
        Some(c) => Ok(c),
        None => Err(soap_error("Encoding: Internal Error")),
    }
}

/// The name of an object's class.
pub(super) fn class_name(req: &Request, o: PhpObject) -> Vec<u8> {
    req.symbols()
        .class_decl(o.class())
        .map(|d| d.name.0.to_vec())
        .unwrap_or_default()
}

/// `zend_value_error`.
pub(super) fn value_error(req: &mut Request, msg: &str) -> Error {
    Error::Thrown(php_ext::raising::value_error(req, msg))
}

/// `(int)` of a `zend_long`, as php's `int` locals take one.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php reads `Z_LVAL_P(ztype)` into an `int`"
)]
pub(super) fn as_int(n: i64) -> i32 {
    n as i32
}

/// `whiteSpace_replace`.
pub(super) fn white_space_replace(s: &[u8]) -> Vec<u8> {
    s.iter()
        .map(|&c| {
            if matches!(c, 0x9 | 0xa | 0xd) {
                b' '
            } else {
                c
            }
        })
        .collect()
}

/// `whiteSpace_collapse`.
pub(super) fn white_space_collapse(s: &[u8]) -> Vec<u8> {
    let s = white_space_replace(s);
    let start = s.iter().position(|&c| c != b' ').unwrap_or(s.len());
    let mut out = Vec::with_capacity(s.len());
    let mut old = 0u8;
    for &c in &s[start..] {
        if c != b' ' || old != b' ' {
            out.push(c);
        }
        old = c;
    }
    if old == b' ' {
        out.pop();
    }
    out
}

/// The text of `data->children->content` collapsed in place, as php's
/// decoders leave it in the tree.
pub(super) fn collapse_in_place(child: Node) -> Vec<u8> {
    let content = child.content().unwrap_or_default();
    let collapsed = white_space_collapse(&content);
    if collapsed != content {
        child.set_content(&collapsed);
    }
    collapsed
}

/// The text of a node's first child, `node->children->content`, where both are there.
pub(super) fn child_content(node: Node) -> Option<Vec<u8>> {
    node.children().and_then(Node::content)
}

// ---- namespaces and types on a node

/// `xmlSearchNsPrefixByHref`: a declaration of `href` with a prefix that
/// the node still sees as that prefix.
fn search_ns_prefix_by_href(orig: Node, href_: &[u8]) -> Option<Ns> {
    let same = |node: Node, ns: Ns| {
        let prefix = ns.prefix();
        prefix.is_some()
            && ns.href().as_deref() == Some(href_)
            && node.search_ns(prefix.as_deref()) == Some(ns)
    };
    let mut at = Some(orig);
    while let Some(node) = at {
        match node.kind() {
            Kind::EntityRef | Kind::Entity | Kind::EntityDecl => return None,
            Kind::Element => {
                let mut cur = node.ns_def();
                while let Some(ns) = cur {
                    if same(node, ns) {
                        return Some(ns);
                    }
                    cur = ns.next();
                }
                if node != orig
                    && let Some(ns) = node.ns()
                    && same(node, ns)
                {
                    return Some(ns);
                }
            }
            _ => {}
        }
        at = node.parent();
    }
    None
}

/// `LIBXML_VERSION < 21300`, read from the library the oracle and the
/// engine both link.
fn libxml_before_2_13() -> bool {
    let v = Library::loaded_version();
    let n = std::str::from_utf8(&v)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);
    n < 21300
}

/// `encode_add_ns`.
pub(crate) fn encode_add_ns(req: &mut Request, node: Node, ns: &[u8]) -> Option<Ns> {
    let doc = node.doc();
    let mut xmlns = node.search_ns_by_href(doc, ns);
    if xmlns.is_some_and(|n| n.prefix().is_none()) {
        xmlns = search_ns_prefix_by_href(node, ns);
    }
    if xmlns.is_some() {
        return xmlns;
    }
    let root = doc.and_then(Node::children);
    if let Some(prefix) = model::default_ns_prefix(ns) {
        return Ns::new(root, Some(ns), Some(prefix));
    }
    let bump = |req: &mut Request| {
        let st = req.state::<State>();
        st.cur_uniq_ns += 1;
        st.cur_uniq_ns
    };
    let mut num = bump(req);
    let prefix = loop {
        let p = format!("ns{num}").into_bytes();
        if node.search_ns(Some(&p)).is_none() {
            break p;
        }
        num = bump(req);
    };
    if libxml_before_2_13() {
        Ns::new(root, Some(&encode_special_chars(ns)), Some(&prefix))
    } else {
        Ns::new(root, Some(ns), Some(&prefix))
    }
}

/// `set_ns_prop`.
pub(super) fn set_ns_prop(req: &mut Request, node: Node, ns: &[u8], name: &[u8], val: &[u8]) {
    let nsp = encode_add_ns(req, node, ns);
    node.set_ns_prop(nsp, name, val);
}

/// `set_xsi_nil`.
pub(super) fn set_xsi_nil(req: &mut Request, node: Node) {
    set_ns_prop(req, node, XSI_NAMESPACE, b"nil", b"true");
}

/// `set_xsi_type`.
pub(super) fn set_xsi_type(req: &mut Request, node: Node, type_: &[u8]) {
    set_ns_prop(req, node, XSI_NAMESPACE, b"type", type_);
}

/// `get_type_str`: `prefix:type`, the namespace declared where it is not.
pub(super) fn get_type_str(
    req: &mut Request,
    node: Node,
    ns: Option<&[u8]>,
    type_: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(mut ns) = ns {
        let v = version(req);
        if v == SOAP_1_2 && ns == SOAP_1_1_ENC_NAMESPACE {
            ns = SOAP_1_2_ENC_NAMESPACE;
        } else if v == SOAP_1_1 && ns == SOAP_1_2_ENC_NAMESPACE {
            ns = SOAP_1_1_ENC_NAMESPACE;
        }
        let prefix = encode_add_ns(req, node, ns).and_then(Ns::prefix);
        out.extend_from_slice(&prefix.unwrap_or_default());
        out.push(b':');
    }
    out.extend_from_slice(type_);
    out
}

/// `set_ns_and_type_ex`.
pub(super) fn set_ns_and_type_ex(req: &mut Request, node: Node, ns: Option<&[u8]>, type_: &[u8]) {
    let nstype = get_type_str(req, node, ns, type_);
    set_xsi_type(req, node, &nstype);
}

/// `set_ns_and_type`.
pub(super) fn set_ns_and_type(req: &mut Request, node: Node, e: &Encoder) {
    let t = e.details.type_str.clone().unwrap_or_default();
    set_ns_and_type_ex(req, node, e.details.ns.as_deref(), &t);
}

// ---- references

/// `soap_check_zval_ref`: a value written before is an `href` (SOAP 1.1)
/// or an `enc:ref` (SOAP 1.2) to the node it became, which gets its id.
fn soap_check_zval_ref(req: &mut Request, cx: &mut Cx, data: &Value, node: Node) -> bool {
    if !cx.tracking {
        return false;
    }
    let ident = match data.as_ref() {
        ValueRef::Object(o) => Some(Ident::Object(o.index())),
        ValueRef::Array(a) => cx
            .slot
            .filter(|r| r.get().as_array().is_some_and(|held| held.is_same(a)))
            .map(Ident::Ref),
        _ => None,
    };
    let Some(ident) = ident else {
        return false;
    };
    let Some(&node_ptr) = cx.sent.get(&ident) else {
        cx.sent.insert(ident, node);
        return false;
    };
    if node_ptr == node {
        return false;
    }
    let v11 = version(req) == SOAP_1_1;
    let attr = if v11 {
        get_attribute(node_ptr.properties(), b"id", None)
    } else {
        get_attribute(node_ptr.properties(), b"id", Some(SOAP_1_2_ENC_NAMESPACE))
    };
    let id = match attr {
        Some(a) => [b"#".as_slice(), &attr_content(a)].concat(),
        None => {
            let st = req.state::<State>();
            st.cur_uniq_ref += 1;
            let id = format!("#ref{}", st.cur_uniq_ref).into_bytes();
            if v11 {
                node_ptr.set_prop(b"id", &id[1..]);
            } else {
                set_ns_prop(req, node_ptr, SOAP_1_2_ENC_NAMESPACE, b"id", &id[1..]);
            }
            id
        }
    };
    if v11 {
        node.set_prop(b"href", &id);
    } else {
        set_ns_prop(req, node, SOAP_1_2_ENC_NAMESPACE, b"ref", &id);
    }
    true
}

/// `soap_check_xml_ref`: the value a node was read as before, one more
/// owner, the caller's.
fn soap_check_xml_ref(req: &mut Request, cx: &Cx, node: Node) -> Option<Owner<Value>> {
    if !cx.tracking {
        return None;
    }
    let v = **cx.read.get(&node)?;
    Some(Owner::take(req, v))
}

/// `soap_add_xml_ref`.
fn soap_add_xml_ref(req: &mut Request, cx: &mut Cx, data: &Value, node: Node) {
    if cx.tracking {
        let kept = Owner::take(req, *data);
        if let Some(old) = cx.read.insert(node, kept) {
            old.give_up(req);
        }
    }
}

/// The first element under `node` and on that carries `attr` (in `ns`)
/// equal to `value`, by any name: `get_node_with_attribute_recursive_ex`
/// with no name.
fn find_with_id(mut node: Option<Node>, value: &[u8], ns: Option<&[u8]>) -> Option<Node> {
    while let Some(n) = node {
        if let Some(a) = get_attribute(n.properties(), b"id", ns)
            && attr_content(a) == value
        {
            return Some(n);
        }
        if let Some(found) = find_with_id(n.children(), value, ns) {
            return Some(found);
        }
        node = n.next();
    }
    None
}

/// `check_and_resolve_href`.
fn check_and_resolve_href(data: Node) -> Res<Node> {
    if data.properties().is_none() {
        return Ok(data);
    }
    let top = data.doc().and_then(Node::children);
    if let Some(h) = get_attribute(data.properties(), b"href", None) {
        let content = attr_content(h);
        if let Some(id) = content.strip_prefix(b"#") {
            return find_with_id(top, id, None).ok_or_else(|| {
                soap_error(format_args!(
                    "Encoding: Unresolved reference '{}'",
                    text(&content)
                ))
            });
        }
        return Err(soap_error(format_args!(
            "Encoding: External reference '{}'",
            text(&content)
        )));
    }
    if let Some(h) = get_attribute(data.properties(), b"ref", Some(SOAP_1_2_ENC_NAMESPACE)) {
        let content = attr_content(h);
        let id = content.strip_prefix(b"#").unwrap_or(&content);
        return match find_with_id(top, id, Some(SOAP_1_2_ENC_NAMESPACE)) {
            None => Err(soap_error(format_args!(
                "Encoding: Unresolved reference '{}'",
                text(&content)
            ))),
            Some(r) if r == data => Err(soap_error(format_args!(
                "Encoding: Violation of id and ref information items '{}'",
                text(&content)
            ))),
            Some(r) => Ok(r),
        };
    }
    Ok(data)
}

// ---- the masters

/// `find_encoder_by_type_name`.
fn find_encoder_by_type_name(sdl: &Sdl, type_: &[u8]) -> Option<EncRef> {
    let encoders = sdl.encoders.as_ref()?;
    encoders
        .values()
        .copied()
        .find(|id| {
            sdl.encoder(*id).is_some_and(|e| {
                if type_.first() == Some(&b'{') {
                    e.details.clark_notation.as_deref() == Some(type_)
                } else {
                    e.details.type_str.as_deref() == Some(type_)
                }
            })
        })
        .map(EncRef::Sdl)
}

/// `master_to_xml`: `data` is a view the function takes no owner of.
pub(crate) fn master_to_xml(
    req: &mut Request,
    cx: &mut Cx,
    enc: Option<EncRef>,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    master_to_xml_int(req, cx, enc, data, style, parent, true)
}

/// `master_to_xml_int`, with `data` held for the length of the call.
fn master_to_xml_int(
    req: &mut Request,
    cx: &mut Cx,
    encode: Option<EncRef>,
    data: Value,
    style: Use,
    parent: Node,
    check_class_map: bool,
) -> Res<Option<Node>> {
    let held = Owner::take(req, data);
    let out = master_to_xml_held(req, cx, encode, data, style, parent, check_class_map);
    held.give_up(req);
    out
}

fn master_to_xml_held(
    req: &mut Request,
    cx: &mut Cx,
    mut encode: Option<EncRef>,
    data: Value,
    style: Use,
    parent: Node,
    check_class_map: bool,
) -> Res<Option<Node>> {
    if let ValueRef::Reference(r) = data.as_ref() {
        cx.slot = Some(r);
    }
    let data = data.deref();
    let sdl = sdl(req);
    if let ValueRef::Object(o) = data.as_ref()
        && Some(o.class()) == lookup_class(req, b"SoapVar")?
    {
        return soap_var_to_xml(req, cx, encode, o, style, parent, sdl.as_ref());
    }
    let mut add_type = false;
    if check_class_map
        && let ValueRef::Object(o) = data.as_ref()
        && !o.is_recursive()
        && let Some(map) = req.state::<State>().class_map
        && let Some(map) = map.deref().as_array()
    {
        let name = class_name(req, o);
        for (key, tmp) in walk::rows(req, map) {
            let php_ext::values::Key::Str(type_name) = key else {
                continue;
            };
            let tmp = tmp.deref();
            let Some(tmp) = tmp.as_str_bytes() else {
                continue;
            };
            if tmp.len() != name.len() || !tmp.eq_ignore_ascii_case(&name) {
                continue;
            }
            let mut found = None;
            if let Some(s) = sdl.as_deref() {
                found = get_encoder(Some(s), s.target_ns.as_deref(), type_name.as_bytes())
                    .or_else(|| find_encoder_by_type_name(s, type_name.as_bytes()));
            }
            if let Some(found) = found {
                if encode != Some(found) && style == Use::Literal {
                    add_type = true;
                }
                encode = Some(found);
            }
            break;
        }
    }
    let encode = match encode {
        Some(e) => e,
        None => get_conversion(ty::UNKNOWN_TYPE)?,
    };
    let mut e = enc(req, encode)?;
    if let Some(t) = typemap_for(req, &e.e) {
        e = enc(req, t)?;
    }
    let node = to_xml(req, cx, &e, data, style, parent)?;
    if add_type && let Some(node) = node {
        set_ns_and_type(req, node, &e.e);
    }
    Ok(node)
}

/// `master_to_xml_int`'s `SoapVar` arm.
fn soap_var_to_xml(
    req: &mut Request,
    cx: &mut Cx,
    encode: Option<EncRef>,
    var: PhpObject,
    style: Use,
    parent: Node,
    sdl: Option<&Arc<Sdl>>,
) -> Res<Option<Node>> {
    let slot = |i| crate::shared::slot(var, i);
    let Some(ztype) = slot(VAR_ENC_TYPE).as_int() else {
        return Err(soap_error("Encoding: SoapVar has no 'enc_type' property"));
    };
    let stype = slot(VAR_ENC_STYPE).as_str_bytes().map(<[u8]>::to_vec);
    let ns = slot(VAR_ENC_NS).as_str_bytes().map(<[u8]>::to_vec);
    let mut found = None;
    if let Some(stype) = &stype {
        found = match &ns {
            Some(ns) => get_encoder(sdl.map(|s| &**s), Some(ns), stype),
            None => get_encoder_ex(sdl.map(|s| &**s), stype),
        };
        if found.is_none() && req.state::<State>().typemap.is_some() {
            found = typemap_find(req, &ns_type(ns.as_deref(), stype));
        }
    }
    let found = match found {
        Some(f) => f,
        None => get_conversion(as_int(ztype))?,
    };
    let value = crate::shared::slot(var, crate::shared::VAR_ENC_VALUE);
    let node = master_to_xml(req, cx, Some(found), value, style, parent)?;
    if (style == Use::Encoded || (sdl.is_some() && encode != Some(found)))
        && let Some(stype) = &stype
        && let Some(node) = node
    {
        set_ns_and_type_ex(req, node, ns.as_deref(), stype);
    }
    if let Some(name) = slot(VAR_ENC_NAME).as_str_bytes()
        && let Some(node) = node
    {
        node.set_name(name);
    }
    if let Some(namens) = slot(VAR_ENC_NAMENS).as_str_bytes().map(<[u8]>::to_vec)
        && let Some(node) = node
    {
        let nsp = encode_add_ns(req, node, &namens);
        node.set_ns_call(nsp);
    }
    Ok(node)
}

/// `master_to_zval`: the value with one owner, the caller's.
pub(crate) fn master_to_zval(
    req: &mut Request,
    cx: &mut Cx,
    enc: Option<EncRef>,
    data: Node,
) -> Res<Owner<Value>> {
    let data = check_and_resolve_href(data)?;
    let encode = match enc {
        None => get_conversion(ty::UNKNOWN_TYPE)?,
        Some(mut encode) => {
            if let Some(type_attr) = get_attribute(data.properties(), b"type", Some(XSI_NAMESPACE))
            {
                let sdl = sdl(req);
                let found = get_encoder_from_prefix(sdl.as_deref(), data, &attr_content(type_attr));
                if let Some(found) = found
                    && found != encode
                    && !loops_back(req, sdl.as_ref(), found)?
                {
                    encode = found;
                }
            }
            encode
        }
    };
    master_to_zval_int(req, cx, encode, data)
}

/// The walk `master_to_zval` and `guess_zval_convert` make down an
/// `xsi:type`'s encoders while their types are not complex: whether it
/// comes back to where it began.
fn loops_back(req: &mut Request, sdl: Option<&Arc<Sdl>>, start: EncRef) -> Res<bool> {
    let mut tmp = Some(start);
    let mut steps = 0u32;
    while let Some(t) = tmp {
        let Some((kind, next)) = chain(req, sdl, t)? else {
            break;
        };
        if kind == TypeKind::Complex {
            break;
        }
        if next == Some(start) || next == Some(t) {
            return Ok(true);
        }
        tmp = next;
        steps += 1;
        if steps > 10_000 {
            break;
        }
    }
    Ok(false)
}

/// `master_to_zval_int`.
fn master_to_zval_int(
    req: &mut Request,
    cx: &mut Cx,
    encode: EncRef,
    data: Node,
) -> Res<Owner<Value>> {
    let mut e = enc(req, encode)?;
    if req.state::<State>().typemap.is_some() {
        if e.e.details.type_str.is_some() {
            if let Some(t) = typemap_for(req, &e.e) {
                e = enc(req, t)?;
            }
        } else if let Some(type_attr) =
            get_attribute(data.properties(), b"type", Some(XSI_NAMESPACE))
        {
            let content = attr_content(type_attr);
            let (cptype, ns) = parse_namespace(&content);
            let nsptr = data.search_ns(ns);
            let key = ns_type(nsptr.map(href).as_deref(), cptype);
            if let Some(t) = typemap_find(req, &key) {
                e = enc(req, t)?;
            }
        }
    }
    to_zval(req, cx, &e, data)
}

// ---- dispatch

/// `encode->to_xml(&encode->details, data, style, parent)`.
fn to_xml(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let some = |n: Node| Ok(Some(n));
    match e.e.to_xml {
        ToXml::Guess => guess_xml_convert(req, cx, data, style, parent),
        ToXml::Null => some(scalar::to_xml_null(req, style, parent)?),
        ToXml::String | ToXml::Duration => {
            some(scalar::to_xml_string(req, e, data, style, parent)?)
        }
        ToXml::Long => some(scalar::to_xml_long(req, e, data, style, parent)?),
        ToXml::Double => some(scalar::to_xml_double(req, e, data, style, parent)?),
        ToXml::Bool => some(scalar::to_xml_bool(req, e, data, style, parent)?),
        ToXml::Base64 => some(scalar::to_xml_base64(req, e, data, style, parent)?),
        ToXml::Hexbin => some(scalar::to_xml_hexbin(req, e, data, style, parent)?),
        ToXml::GuessArrayMap => array::guess_array_map(req, cx, data, style, parent),
        ToXml::Array => some(array::to_xml_array(req, cx, e, data, style, parent)?),
        ToXml::Map => array::to_xml_map(req, cx, e, data, style, parent),
        ToXml::Object => some(object::to_xml_object(req, cx, e, data, style, parent)?),
        ToXml::List1 => some(scalar::to_xml_list(req, cx, e, data, style, parent)?),
        ToXml::Datetime => some(time::to_xml_datetime(req, e, data, style, parent)?),
        ToXml::Time => some(time::to_xml_time(req, e, data, style, parent)?),
        ToXml::Date => some(time::to_xml_date(req, e, data, style, parent)?),
        ToXml::GYearMonth => some(time::to_xml_gyearmonth(req, e, data, style, parent)?),
        ToXml::GYear => some(time::to_xml_gyear(req, e, data, style, parent)?),
        ToXml::GMonthDay => some(time::to_xml_gmonthday(req, e, data, style, parent)?),
        ToXml::GDay => some(time::to_xml_gday(req, e, data, style, parent)?),
        ToXml::GMonth => some(time::to_xml_gmonth(req, e, data, style, parent)?),
        ToXml::Any => scalar::to_xml_any(req, cx, data, style, parent),
        ToXml::SdlGuess => sdl_guess_convert_xml(req, cx, e, data, style, parent),
        ToXml::User => some(scalar::to_xml_user(req, e, data, style, parent)?),
    }
}

/// `encode->to_zval(ret, &encode->details, data)`.
fn to_zval(req: &mut Request, cx: &mut Cx, e: &Enc, data: Node) -> Res<Owner<Value>> {
    match e.e.to_zval {
        ToZval::Guess => guess_zval_convert(req, cx, e, data),
        ToZval::Null => Ok(Owner::null()),
        ToZval::String => scalar::to_zval_string(req, data, scalar::Space::Keep),
        ToZval::StringR => scalar::to_zval_string(req, data, scalar::Space::Replace),
        ToZval::StringC => scalar::to_zval_string(req, data, scalar::Space::Collapse),
        ToZval::Long => Ok(Owner::take(req, scalar::to_zval_long(data)?)),
        ToZval::Double => Ok(Owner::take(req, scalar::to_zval_double(data)?)),
        ToZval::Bool => Ok(Owner::take(req, scalar::to_zval_bool(data)?)),
        ToZval::Base64 => scalar::to_zval_base64(req, data),
        ToZval::Hexbin => scalar::to_zval_hexbin(req, data),
        ToZval::Array => array::to_zval_array(req, cx, e, data),
        ToZval::Map => array::to_zval_map(req, cx, data),
        ToZval::Object => object::to_zval_object(req, cx, e, data),
        ToZval::Any => scalar::to_zval_any(req, cx, data),
        ToZval::SdlGuess => sdl_guess_convert_zval(req, cx, e, data),
        ToZval::User => scalar::to_zval_user(req, e, data),
    }
}

// ---- guesses

/// `guess_xml_convert`.
fn guess_xml_convert(
    req: &mut Request,
    cx: &mut Cx,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let code = if matches!(data.as_ref(), ValueRef::Undef) {
        ty::IS_NULL
    } else {
        type_code(&data.deref())
    };
    let e = get_conversion(code)?;
    master_to_xml_int(req, cx, Some(e), data, style, parent, false)
}

/// `guess_zval_convert`.
fn guess_zval_convert(
    req: &mut Request,
    cx: &mut Cx,
    type_: &Enc,
    data: Node,
) -> Res<Owner<Value>> {
    let data = check_and_resolve_href(data)?;
    let sdl = sdl(req);
    let mut type_name = None;
    let found = if xml_nil(data) {
        get_conversion(ty::IS_NULL)?
    } else {
        let mut found = None;
        if let Some(attr) = get_attribute(data.properties(), b"type", Some(XSI_NAMESPACE)) {
            let name = attr_content(attr);
            found = get_encoder_from_prefix(sdl.as_deref(), data, &name);
            type_name = Some(name);
            if found == Some(type_.r) {
                found = None;
            }
            if let Some(f) = found
                && loops_back(req, sdl.as_ref(), f)?
            {
                found = None;
            }
        }
        match found {
            Some(f) => f,
            None => {
                let props = data.properties();
                if array::get_soap_enc_attribute(props, b"arrayType").is_some()
                    || array::get_soap_enc_attribute(props, b"itemType").is_some()
                    || array::get_soap_enc_attribute(props, b"arraySize").is_some()
                {
                    get_conversion(ty::SOAP_ENC_ARRAY)?
                } else if crate::xml::children(data).any(|t| t.kind() == Kind::Element) {
                    get_conversion(ty::SOAP_ENC_OBJECT)?
                } else {
                    get_conversion(ty::XSD_STRING)?
                }
            }
        }
    };
    let ret = master_to_zval_int(req, cx, found, data)?;
    let Some(type_name) = type_name else {
        return Ok(ret);
    };
    let e = match enc(req, found) {
        Ok(e) => e,
        Err(err) => {
            ret.give_up(req);
            return Err(err);
        }
    };
    if sdl.is_none() || e.e.details.sdl_type.is_none() {
        return Ok(ret);
    }
    wrap_soap_var(req, &e.e, ret, &type_name, data)
}

/// `guess_zval_convert`'s `SoapVar` around what was read.
fn wrap_soap_var(
    req: &mut Request,
    e: &Encoder,
    ret: Owner<Value>,
    type_name: &[u8],
    data: Node,
) -> Res<Owner<Value>> {
    let made = class(req, "SoapVar").and_then(|class| Ok(php_ext::call::new_object(req, class)?));
    let var = match made {
        Ok(v) => v,
        Err(err) => {
            ret.give_up(req);
            return Err(err);
        }
    };
    let (cptype, ns) = parse_namespace(type_name);
    let nsptr = data.search_ns(ns);
    let fill = |req: &mut Request| -> Res<()> {
        if let Err(err) = var.set_slot(req, VAR_ENC_TYPE, Owner::int(i64::from(e.details.type_))) {
            ret.give_up(req);
            return Err(err.into());
        }
        var.set_slot(req, crate::shared::VAR_ENC_VALUE, ret)?;
        let s = string(req, cptype)?;
        var.set_slot(req, VAR_ENC_STYPE, s)?;
        if let Some(n) = nsptr {
            let s = string(req, &href(n))?;
            var.set_slot(req, VAR_ENC_NS, s)?;
        }
        Ok(())
    };
    match fill(req) {
        Ok(()) => Ok(var.into_value()),
        Err(err) => {
            var.give_up(req);
            Err(err)
        }
    }
}

/// `sdl_guess_convert_zval`.
fn sdl_guess_convert_zval(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Node,
) -> Res<Owner<Value>> {
    let sdl = sdl(req);
    let Some(t) = sdl_type(sdl.as_ref(), &e.e) else {
        return guess_zval_convert(req, cx, e, data);
    };
    match t.kind {
        TypeKind::Simple => match t.encode {
            Some(te) if te != e.r => master_to_zval_int(req, cx, te, data),
            _ => guess_zval_convert(req, cx, e, data),
        },
        TypeKind::List | TypeKind::Union => {
            scalar::to_zval_string(req, data, scalar::Space::Collapse)
        }
        TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension => {
            if is_array_encoder(req, t.encode)? {
                array::to_zval_array(req, cx, e, data)
            } else {
                object::to_zval_object(req, cx, e, data)
            }
        }
    }
}

/// `type->encode && (type->encode->details.type == IS_ARRAY || ... == SOAP_ENC_ARRAY)`.
pub(super) fn is_array_encoder(req: &mut Request, r: Option<EncRef>) -> Res<bool> {
    let Some(r) = r else {
        return Ok(false);
    };
    let t = enc(req, r)?.e.details.type_;
    Ok(t == ty::IS_ARRAY || t == ty::SOAP_ENC_ARRAY)
}

/// `sdl_guess_convert_xml`.
fn sdl_guess_convert_xml(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let sdl = sdl(req);
    let Some(t) = sdl_type(sdl.as_ref(), &e.e) else {
        let ret = guess_xml_convert(req, cx, data, style, parent)?;
        if style == Use::Encoded
            && let Some(ret) = ret
        {
            set_ns_and_type(req, ret, &e.e);
        }
        return Ok(ret);
    };
    let ret = match t.kind {
        TypeKind::Simple => match t.encode {
            Some(te) if te != e.r => master_to_xml(req, cx, Some(te), data, style, parent)?,
            _ => guess_xml_convert(req, cx, data, style, parent)?,
        },
        TypeKind::List | TypeKind::Union => {
            Some(scalar::to_xml_list(req, cx, e, data, style, parent)?)
        }
        TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension => {
            return if is_array_encoder(req, t.encode)? {
                Ok(Some(array::to_xml_array(req, cx, e, data, style, parent)?))
            } else {
                Ok(Some(object::to_xml_object(
                    req, cx, e, data, style, parent,
                )?))
            };
        }
    };
    if style == Use::Encoded
        && let Some(ret) = ret
    {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

#[cfg(test)]
mod tests {
    use super::{white_space_collapse, white_space_replace};

    #[test]
    fn collapse_as_php() {
        assert_eq!(white_space_collapse(b"  a \t\n b  "), b"a b");
        assert_eq!(white_space_collapse(b"   "), b"");
        assert_eq!(white_space_collapse(b"x"), b"x");
        assert_eq!(white_space_replace(b"a\r\nb"), b"a  b");
    }
}

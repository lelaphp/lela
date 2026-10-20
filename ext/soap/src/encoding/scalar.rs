//! `php_encoding.c`'s strings, numbers, booleans, `null`, binary, lists,
//! `<anyXML>` and the typemap's user converters.

use php_ext::call::call;
use php_ext::coerce::{
    Exponent, callable_checked, float_format, numeric_string, to_double, to_long, to_string, truthy,
};
use php_ext::decl::{ClassKind, EnumBacking};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{Kind, Library, Node, Save};

use super::{
    Cx, Enc, bogus, child_content, collapse_in_place, find_zval_null, master_to_xml,
    master_to_zval_int, new_node, sdl, set_ns_and_type, string, text, white_space_collapse,
    white_space_replace, xml_nil,
};
use crate::model::{TypeKind, Use, ty};
use crate::xml::parse_memory;
use crate::{Res, State, soap_error};

/// `Encoding: Violation of encoding rules`.
fn violation() -> crate::Error {
    soap_error("Encoding: Violation of encoding rules")
}

/// What a string decoder does to the text's white space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Space {
    /// `to_zval_string`.
    Keep,
    /// `to_zval_stringr`.
    Replace,
    /// `to_zval_stringc`.
    Collapse,
}

/// `SOAP_GLOBAL(encoding)`.
fn encoding(req: &mut Request) -> Option<Vec<u8>> {
    req.state::<State>().encoding.clone()
}

/// `to_zval_string`, `to_zval_stringr` and `to_zval_stringc`.
pub(super) fn to_zval_string(req: &mut Request, data: Node, space: Space) -> Res<Owner<Value>> {
    if xml_nil(data) {
        return Ok(Owner::null());
    }
    let Some(child) = data.children() else {
        return string(req, b"");
    };
    let single = child.next().is_none();
    match child.kind() {
        Kind::Text if single => {
            let content = match space {
                Space::Keep => child.content().unwrap_or_default(),
                Space::Replace => {
                    let content = child.content().unwrap_or_default();
                    let replaced = white_space_replace(&content);
                    if replaced != content {
                        child.set_content(&replaced);
                    }
                    replaced
                }
                Space::Collapse => collapse_in_place(child),
            };
            let out = match encoding(req) {
                Some(enc) => Library::from_utf8(&enc, &content).unwrap_or(content),
                None => content,
            };
            string(req, &out)
        }
        Kind::CData if single => string(req, &child.content().unwrap_or_default()),
        _ => Err(violation()),
    }
}

/// `to_zval_base64`.
pub(super) fn to_zval_base64(req: &mut Request, data: Node) -> Res<Owner<Value>> {
    if xml_nil(data) {
        return Ok(Owner::null());
    }
    let Some(child) = data.children() else {
        return string(req, b"");
    };
    let content = match child.kind() {
        Kind::Text if child.next().is_none() => collapse_in_place(child),
        Kind::CData if child.next().is_none() => child.content().unwrap_or_default(),
        _ => return Err(violation()),
    };
    match php_ext_standard::base64::decode(&content, false) {
        Some(bytes) => string(req, &bytes),
        None => Err(violation()),
    }
}

/// One hex digit's value.
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// `to_zval_hexbin`'s loop: pairs of digits, a last odd one dropped.
fn hex_decode(content: &[u8]) -> Option<Vec<u8>> {
    content
        .chunks_exact(2)
        .map(|p| Some((hex_digit(p[0])? << 4) | hex_digit(p[1])?))
        .collect()
}

/// `to_zval_hexbin`.
pub(super) fn to_zval_hexbin(req: &mut Request, data: Node) -> Res<Owner<Value>> {
    if xml_nil(data) {
        return Ok(Owner::null());
    }
    let Some(child) = data.children() else {
        return string(req, b"");
    };
    let content = match child.kind() {
        Kind::Text if child.next().is_none() => collapse_in_place(child),
        Kind::CData if child.next().is_none() => child.content().unwrap_or_default(),
        _ => return Err(violation()),
    };
    match hex_decode(&content) {
        Some(bytes) => string(req, &bytes),
        None => Err(violation()),
    }
}

/// The text a number decoder reads: one text child, collapsed in place.
fn number_text(data: Node) -> Res<Option<Vec<u8>>> {
    let Some(child) = data.children() else {
        return Ok(None);
    };
    if child.kind() != Kind::Text || child.next().is_some() {
        return Err(violation());
    }
    Ok(Some(collapse_in_place(child)))
}

/// `strncasecmp(s, prefix, strlen(prefix)) == 0`.
fn starts_ci(s: &[u8], prefix: &[u8]) -> bool {
    s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// `to_zval_double`.
pub(super) fn to_zval_double(data: Node) -> Res<Value> {
    if xml_nil(data) {
        return Ok(Value::null());
    }
    let Some(content) = number_text(data)? else {
        return Ok(Value::null());
    };
    let n = numeric_string(&content);
    match n.as_ref().map(Value::as_ref) {
        Some(ValueRef::Int(n)) => Ok(Value::float(int_to_double(n))),
        Some(ValueRef::Float(f)) => Ok(Value::float(f)),
        _ if starts_ci(&content, b"NaN") => Ok(Value::float(f64::NAN)),
        _ if starts_ci(&content, b"INF") => Ok(Value::float(f64::INFINITY)),
        _ if starts_ci(&content, b"-INF") => Ok(Value::float(f64::NEG_INFINITY)),
        _ => Err(violation()),
    }
}

/// `to_zval_long`.
pub(super) fn to_zval_long(data: Node) -> Res<Value> {
    if xml_nil(data) {
        return Ok(Value::null());
    }
    let Some(content) = number_text(data)? else {
        return Ok(Value::null());
    };
    let n = numeric_string(&content);
    match n.as_ref().map(Value::as_ref) {
        Some(ValueRef::Int(n)) => Ok(Value::int(n)),
        Some(ValueRef::Float(f)) => Ok(Value::float(f)),
        _ => Err(violation()),
    }
}

/// `(double) lval`.
#[expect(clippy::cast_precision_loss, reason = "php's `(double) lval`")]
fn int_to_double(n: i64) -> f64 {
    n as f64
}

/// `to_zval_bool`.
pub(super) fn to_zval_bool(data: Node) -> Res<Value> {
    if xml_nil(data) {
        return Ok(Value::null());
    }
    let Some(child) = data.children() else {
        return Ok(Value::null());
    };
    if child.kind() != Kind::Text || child.next().is_some() {
        return Err(violation());
    }
    let c = collapse_in_place(child);
    let f = c.is_empty()
        || c.as_slice() == b"0"
        || c.eq_ignore_ascii_case(b"f")
        || c.eq_ignore_ascii_case(b"false");
    Ok(Value::bool(!f))
}

// ---- to XML

/// The enum case's class kind and backing, where `v` is a case.
fn enum_backing(req: &Request, v: &Value) -> Option<EnumBacking> {
    let o = v.as_object()?;
    let decl = req.class_decl(o.class())?;
    if decl.kind != ClassKind::Enum {
        return None;
    }
    // `enum_backing_type`, read off the case's `value` as a user enum's
    // declaration does not carry it.
    Some(match o.prop(req, b"value").map(|v| v.deref()) {
        None => EnumBacking::Pure,
        Some(v) if v.as_int().is_some() => EnumBacking::Int,
        Some(_) => EnumBacking::String,
    })
}

/// `zend_value_error` where php goes on with the throwable pending: the
/// `ValueError` chained after any already pending, kept in
/// `State::pending` for the message's maker to hand on once the message
/// is made, as php's caller meets `EG(exception)`.
fn left_pending(req: &mut Request, msg: &str) {
    let thrown = php_ext::raising::throw_error(req, "ValueError", msg);
    let earlier = req.state::<crate::State>().pending.take();
    let now = match earlier {
        Some(e) => e.chain(req, thrown),
        None => thrown,
    };
    req.state::<crate::State>().pending = Some(now);
}

/// `get_serialization_string_from_zval`: the string, one owner, the caller's.
pub(super) fn serialization_string(req: &mut Request, data: &Value) -> Res<Owner<PhpString>> {
    let data = data.deref();
    match enum_backing(req, &data) {
        Some(EnumBacking::Pure) => {
            left_pending(req, "Non-backed enums have no default serialization");
            Ok(PhpString::new(req, b"")?)
        }
        Some(_) => {
            let case = data
                .as_object()
                .and_then(|o| o.prop(req, b"value"))
                .unwrap_or_default();
            Ok(to_string(req, &case)?)
        }
        None if matches!(data.as_ref(), ValueRef::Undef) => Ok(PhpString::new(req, b"")?),
        None => Ok(to_string(req, &data)?),
    }
}

/// [`serialization_string`]'s bytes, the string given back.
pub(super) fn serialization_bytes(req: &mut Request, data: &Value) -> Res<Vec<u8>> {
    let s = serialization_string(req, data)?;
    let out = s.as_bytes().to_vec();
    s.give_up(req);
    Ok(out)
}

/// `get_serialization_long_from_zval`.
fn serialization_long(req: &mut Request, data: &Value) -> Res<i64> {
    match enum_backing(req, data) {
        Some(EnumBacking::Int) => Ok(data
            .as_object()
            .and_then(|o| o.prop(req, b"value"))
            .and_then(|v| v.as_int())
            .unwrap_or(0)),
        Some(EnumBacking::Pure) => {
            left_pending(req, "Non-backed enums have no default serialization");
            Ok(0)
        }
        Some(EnumBacking::String) => {
            left_pending(req, "String-backed enum cannot be serialized as int");
            Ok(0)
        }
        None => Ok(to_long(req, data)?),
    }
}

/// `xmlCheckUTF8`, over the bytes before the first NUL.
pub(super) fn check_utf8(s: &[u8]) -> bool {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut ix = 0;
    loop {
        let c = at(ix);
        if c == 0 {
            return true;
        }
        if c & 0x80 == 0 {
            ix += 1;
        } else if c & 0xe0 == 0xc0 {
            if at(ix + 1) & 0xc0 != 0x80 {
                return false;
            }
            ix += 2;
        } else if c & 0xf0 == 0xe0 {
            if at(ix + 1) & 0xc0 != 0x80 || at(ix + 2) & 0xc0 != 0x80 {
                return false;
            }
            ix += 3;
        } else if c & 0xf8 == 0xf0 {
            if at(ix + 1) & 0xc0 != 0x80 || at(ix + 2) & 0xc0 != 0x80 || at(ix + 3) & 0xc0 != 0x80 {
                return false;
            }
            ix += 4;
        } else {
            return false;
        }
    }
}

/// `to_xml_string`'s message for a string that is no UTF-8: the bytes up
/// to the first bad one, that one as `\xNN`, and `...`.
pub(super) fn bad_utf8(s: &[u8]) -> Vec<u8> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let mut i = 0;
    let mut c;
    loop {
        c = at(i);
        i += 1;
        if c == 0 {
            break;
        }
        if c & 0x80 == 0 {
        } else if c & 0xe0 == 0xc0 {
            if at(i) & 0xc0 != 0x80 {
                break;
            }
            i += 1;
        } else if c & 0xf0 == 0xe0 {
            if at(i) & 0xc0 != 0x80 || at(i + 1) & 0xc0 != 0x80 {
                break;
            }
            i += 2;
        } else if c & 0xf8 == 0xf0 {
            if at(i) & 0xc0 != 0x80 || at(i + 1) & 0xc0 != 0x80 || at(i + 2) & 0xc0 != 0x80 {
                break;
            }
            i += 3;
        } else {
            break;
        }
    }
    if c == 0 {
        return php_ext::coerce::c_str(s).to_vec();
    }
    let hex = |d: u8| if d > 9 { b'a' + d - 10 } else { b'0' + d };
    let mut out = s[..(i - 1).min(s.len())].to_vec();
    out.extend_from_slice(&[b'\\', b'x', hex(c >> 4), hex(c & 15), b'.', b'.', b'.']);
    out
}

/// `to_xml_string`, and `to_xml_duration`, which is it.
pub(super) fn to_xml_string(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    let mut s = serialization_bytes(req, &data)?;
    if let Some(enc) = encoding(req)
        && let Some(converted) = Library::to_utf8(&enc, &s)
    {
        s = converted;
    }
    if !check_utf8(&s) {
        let err = bad_utf8(&s);
        return Err(soap_error(format_args!(
            "Encoding: string '{}' is not a valid utf-8 string",
            text(&err)
        )));
    }
    if let Some(t) = Node::new_text_len(None, &s) {
        ret.add_child(t);
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_base64`.
pub(super) fn to_xml_base64(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    let s = serialization_bytes(req, &data)?;
    let encoded = php_ext_standard::base64::encode(&s);
    if let Some(t) = Node::new_text_len(None, &encoded) {
        ret.add_child(t);
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_hexbin`.
pub(super) fn to_xml_hexbin(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    let s = match data.as_str_bytes() {
        Some(s) => s.to_vec(),
        None => serialization_bytes(req, &data)?,
    };
    let mut out = Vec::with_capacity(s.len() * 2);
    for &c in &s {
        out.push(HEX[usize::from(c >> 4)]);
        out.push(HEX[usize::from(c & 15)]);
    }
    if let Some(t) = Node::new_text_len(None, &out) {
        ret.add_child(t);
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `snprintf("%0.0F", floor(d))`.
fn fixed_floor(d: f64) -> String {
    let f = d.floor();
    if f.is_nan() {
        if f.is_sign_negative() {
            "-NAN".into()
        } else {
            "NAN".into()
        }
    } else if f.is_infinite() {
        if f < 0.0 { "-INF".into() } else { "INF".into() }
    } else {
        format!("{f:.0}")
    }
}

/// `to_xml_long`.
pub(super) fn to_xml_long(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    if let Some(d) = data.as_float() {
        ret.set_content(fixed_floor(d).as_bytes());
    } else {
        let n = serialization_long(req, &data)?;
        ret.set_content_len(n.to_string().as_bytes());
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_double`.
pub(super) fn to_xml_double(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    let d = to_double(req, &data)?;
    let s = float_format(d, req.ini().precision(), Exponent::Upper);
    ret.set_content_len(s.as_bytes());
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_bool`.
pub(super) fn to_xml_bool(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    if truthy(req, &data)? {
        ret.set_content(b"true");
    } else {
        ret.set_content(b"false");
    }
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_xml_null`.
pub(super) fn to_xml_null(req: &mut Request, style: Use, parent: Node) -> Res<Node> {
    let ret = bogus(parent)?;
    if style == Use::Encoded {
        super::set_xsi_nil(req, ret);
    }
    Ok(ret)
}

// ---- lists

/// `to_xml_list`, which `to_xml_list1` and `to_xml_union` are.
pub(super) fn to_xml_list(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let sdl = sdl(req);
    let list_enc = super::sdl_type(sdl.as_ref(), &e.e)
        .filter(|t| t.kind == TypeKind::List)
        .and_then(|t| t.elements.as_ref())
        .and_then(|els| els.first().copied())
        .and_then(|id| sdl.as_ref()?.ty(id)?.encode);
    let ret = bogus(parent)?;
    if find_zval_null(req, &data, ret, style) {
        return Ok(ret);
    }
    let was = cx.tracking;
    cx.tracking = false;
    let out = list_items(req, cx, list_enc, data, ret);
    cx.tracking = was;
    let list = out?;
    ret.set_content_len(&list);
    Ok(ret)
}

/// `to_xml_list`'s items, each written and read back as text.
fn list_items(
    req: &mut Request,
    cx: &mut Cx,
    list_enc: Option<crate::model::EncRef>,
    data: Value,
    ret: Node,
) -> Res<Vec<u8>> {
    let mut list = Vec::new();
    let mut one = |req: &mut Request, cx: &mut Cx, v: Value| -> Res<()> {
        let dummy = master_to_xml(req, cx, list_enc, v, Use::Literal, ret)?;
        let content = dummy.and_then(child_content);
        let Some(content) = content else {
            return Err(violation());
        };
        if !list.is_empty() {
            list.push(b' ');
        }
        list.extend_from_slice(&content);
        if let Some(d) = dummy {
            d.unlink();
            d.free();
        }
        Ok(())
    };
    if let Some(arr) = data.as_array().copied() {
        let read = super::array::rows(req, arr);
        super::array::with_rows(req, &read, |req, rows| {
            for (_, v) in rows {
                one(req, cx, *v)?;
            }
            Ok(())
        })?;
    } else {
        let s = match data.as_str_bytes() {
            Some(s) => s.to_vec(),
            None => serialization_bytes(req, &data)?,
        };
        let collapsed = white_space_collapse(php_ext::coerce::c_str(&s));
        if !collapsed.is_empty() {
            for item in collapsed.split(|&c| c == b' ') {
                let v = string(req, item)?;
                let r = one(req, cx, *v);
                v.give_up(req);
                r?;
            }
        }
    }
    Ok(list)
}

// ---- `<anyXML>`

/// `to_zval_any`.
pub(super) fn to_zval_any(req: &mut Request, cx: &mut Cx, data: Node) -> Res<Owner<Value>> {
    if let Some(s) = sdl(req)
        && let Some(elements) = s.elements.as_ref()
        && data.has_name()
    {
        let mut key = Vec::new();
        if let Some(h) = data.ns().and_then(php_ext_libxml::ffi::Ns::href) {
            key.extend_from_slice(&h);
            key.push(b':');
        }
        key.extend_from_slice(&data.name());
        if let Some(t) = elements.get_str(&key).and_then(|id| s.ty(*id))
            && let Some(te) = t.encode
        {
            return master_to_zval_int(req, cx, te, data);
        }
    }
    let dumped = Save::bare(data).unwrap_or_default();
    string(req, php_ext::coerce::c_str(&dumped))
}

/// `to_xml_any`.
pub(super) fn to_xml_any(
    req: &mut Request,
    cx: &mut Cx,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    if let Some(arr) = data.as_array().copied() {
        let enc = super::get_conversion(ty::XSD_ANYXML)?;
        let read = super::array::rows(req, arr);
        return super::array::with_rows(req, &read, |req, rows| {
            let mut ret = None;
            for (k, el) in rows {
                ret = master_to_xml(req, cx, Some(enc), *el, style, parent)?;
                if let Some(r) = ret
                    && !r.is_text_noenc()
                    && let super::array::K::Str(name) = k
                {
                    r.set_name(name);
                }
            }
            Ok(ret)
        });
    }
    let s = serialization_bytes(req, &data)?;
    let Some(ret) = Node::new_text_noenc(&s) else {
        return Err(soap_error("Encoding: Internal Error"));
    };
    let last = parent.last();
    ret.set_parent(Some(parent));
    ret.set_tree_doc(parent.doc());
    ret.set_prev(last);
    ret.set_next(None);
    match last {
        Some(l) => l.set_next(Some(ret)),
        None => parent.set_children(Some(ret)),
    }
    parent.set_last(Some(ret));
    Ok(Some(ret))
}

// ---- the typemap's converters

/// The typemap entry's `to_xml` or `from_xml` of an encoder.
fn map_fn(req: &mut Request, e: &Enc, to_xml: bool) -> Option<Value> {
    let crate::model::EncRef::Typemap(n) = e.r else {
        return None;
    };
    let entry = req.state::<State>().typemap.as_ref()?.nth(n as usize)?;
    if to_xml { entry.to_xml } else { entry.to_zval }
}

/// `to_xml_user`.
pub(super) fn to_xml_user(
    req: &mut Request,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let mut ret = None;
    if let Some(f) = map_fn(req, e, true) {
        let Ok(callable) = callable_checked(req, &f) else {
            return Err(soap_error("Encoding: Error calling to_xml callback"));
        };
        let arg = if matches!(data.as_ref(), ValueRef::Undef) {
            Value::null()
        } else {
            data
        };
        let answer = call(req, &callable, &[arg])?;
        if let Some(s) = answer.as_str_bytes().map(<[u8]>::to_vec) {
            let parsed = parse_memory(req, &s);
            answer.give_up(req);
            let doc = parsed?;
            if let Some(doc) = doc {
                if let Some(first) = doc.children() {
                    ret = first.copy(parent.doc(), 1);
                }
                doc.free_document();
            }
        } else {
            answer.give_up(req);
        }
    }
    let ret = match ret {
        Some(r) => r,
        None => new_node(parent.doc(), b"BOGUS")?,
    };
    parent.add_child(ret);
    if style == Use::Encoded {
        set_ns_and_type(req, ret, &e.e);
    }
    Ok(ret)
}

/// `to_zval_user`.
pub(super) fn to_zval_user(req: &mut Request, e: &Enc, node: Node) -> Res<Owner<Value>> {
    let Some(f) = map_fn(req, e, false) else {
        return Ok(Owner::null());
    };
    let dumped = match node.copy(None, 1) {
        Some(copy) => {
            let d = Save::bare(copy).unwrap_or_default();
            copy.free();
            d
        }
        None => Vec::new(),
    };
    let Ok(callable) = callable_checked(req, &f) else {
        return Err(soap_error("Encoding: Error calling from_xml callback"));
    };
    let arg = string(req, php_ext::coerce::c_str(&dumped))?;
    let out = call(req, &callable, &[*arg]);
    arg.give_up(req);
    Ok(out?)
}

#[cfg(test)]
mod tests {
    use super::{bad_utf8, check_utf8, fixed_floor, hex_decode};

    #[test]
    fn utf8_as_libxml() {
        assert!(check_utf8("aé€😀".as_bytes()));
        assert!(!check_utf8(b"a\xffb"));
        assert!(check_utf8(b"a\0\xff"));
        assert_eq!(bad_utf8(b"ab\xffcd"), b"ab\\xff...");
        assert_eq!(bad_utf8(b"\xc3("), b"\\xc3...");
    }

    #[test]
    fn floor_as_printf() {
        assert_eq!(fixed_floor(1.9), "1");
        assert_eq!(fixed_floor(-1.5), "-2");
        assert_eq!(fixed_floor(1e20), "100000000000000000000");
        assert_eq!(fixed_floor(f64::INFINITY), "INF");
        assert_eq!(fixed_floor(-0.0), "-0");
    }

    #[test]
    fn hex_pairs() {
        assert_eq!(hex_decode(b"4a6B"), Some(b"Jk".to_vec()));
        assert_eq!(hex_decode(b"4a6"), Some(b"J".to_vec()));
        assert_eq!(hex_decode(b"zz"), None);
    }
}

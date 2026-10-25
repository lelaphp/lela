//! `php_encoding.c`'s arrays: SOAP encoding's `Array` with its
//! `arrayType`, `itemType`, `arraySize`, `offset` and `position`, the
//! Apache `Map`, and the guess between them.

use php_ext::call::{iterate, lookup_class};
use php_ext::coerce::key_of;
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, Value, ValueRef, walk};
use php_ext_libxml::ffi::{Kind, Node, Ns};

use super::{
    Cx, Enc, as_int, conv, enc, encode_add_ns, find_zval_null, get_conversion, get_type_str,
    master_to_xml, master_to_zval, new_node, sdl, sdl_type, set_ns_and_type, set_ns_and_type_ex,
    set_ns_prop, set_xsi_type, type_code, value_error, version, xml_nil,
};
use crate::model::{
    EncRef, SOAP_1_1_ENC_NAMESPACE, SOAP_1_2_ENC_NAMESPACE, Sdl, Type, TypeId, Use, WSDL_NAMESPACE,
    ty,
};
use crate::sdl::{get_encoder, get_encoder_ex};
use crate::shared::{VAR_ENC_NS, VAR_ENC_STYPE, VAR_ENC_TYPE, VAR_ENC_VALUE};
use crate::xml::{attr_content, get_attribute, get_node, parse_namespace};
use crate::{Res, SOAP_1_1, soap_error};

/// `SOAP_USE_XSI_ARRAY_TYPE`.
const SOAP_USE_XSI_ARRAY_TYPE: i64 = 4;

/// An array key as a walk reads it, kept past the walk.
#[derive(Debug, Clone)]
pub(super) enum K {
    Int(i64),
    Str(Vec<u8>),
}

/// The elements of an array in order, references kept, read under the
/// borrowed request ([`walk::rows`]): views, which [`with_rows`] holds
/// while a program's converter runs.
pub(super) fn rows(req: &Request, arr: PhpArray) -> Vec<(K, Value)> {
    walk::rows(req, arr)
        .map(|(k, v)| {
            let k = match k {
                Key::Int(n) => K::Int(n),
                Key::Str(s) => K::Str(s.as_bytes().to_vec()),
            };
            (k, v)
        })
        .collect()
}

/// `ZEND_HASH_FOREACH` over `rows`, each value held for the walk so a
/// program's converter cannot free one under it.
pub(super) fn with_rows<T>(
    req: &mut Request,
    rows: &[(K, Value)],
    f: impl FnOnce(&mut Request, &[(K, Value)]) -> Res<T>,
) -> Res<T> {
    let held: Vec<Owner<Value>> = rows.iter().map(|(_, v)| Owner::take(req, *v)).collect();
    let out = f(req, rows);
    for v in held {
        v.give_up(req);
    }
    out
}

/// `get_soap_enc_attribute`: SOAP 1.1's encoding namespace, then 1.2's.
pub(super) fn get_soap_enc_attribute(props: Option<Node>, name: &[u8]) -> Option<Node> {
    get_attribute(props, name, Some(SOAP_1_1_ENC_NAMESPACE))
        .or_else(|| get_attribute(props, name, Some(SOAP_1_2_ENC_NAMESPACE)))
}

/// `guess_array_map`.
pub(super) fn guess_array_map(
    req: &mut Request,
    cx: &mut Cx,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let e = match data.as_array() {
        Some(a) if !a.is_list() => get_conversion(ty::APACHE_MAP)?,
        Some(_) => get_conversion(ty::SOAP_ENC_ARRAY)?,
        None => get_conversion(ty::IS_NULL)?,
    };
    master_to_xml(req, cx, Some(e), data, style, parent)
}

// ---- dimensions and positions

/// `calc_dimension_12`.
fn calc_dimension_12(s: &[u8]) -> Res<usize> {
    let mut i = 0;
    let mut at = s
        .iter()
        .position(|&c| c.is_ascii_digit() || c == b'*')
        .unwrap_or(s.len());
    if s.get(at) == Some(&b'*') {
        i += 1;
        at += 1;
    }
    let mut flag = false;
    for &c in &s[at..] {
        if c.is_ascii_digit() {
            if !flag {
                i += 1;
                flag = true;
            }
        } else if c == b'*' {
            return Err(soap_error(
                "Encoding: '*' may only be first arraySize value in list",
            ));
        } else {
            flag = false;
        }
    }
    Ok(i)
}

/// `soap_array_position_add_digit`.
fn add_digit(position: &mut i32, digit: i32) -> Res<()> {
    if *position > (i32::MAX - digit) / 10 {
        return Err(soap_error("Encoding: array index out of range"));
    }
    *position = *position * 10 + digit;
    Ok(())
}

/// `get_position_12`.
fn get_position_12(dimension: usize, s: &[u8]) -> Res<Vec<i32>> {
    let mut pos = vec![0; dimension];
    let mut at = s
        .iter()
        .position(|&c| c.is_ascii_digit() || c == b'*')
        .unwrap_or(s.len());
    let mut i: Option<usize> = None;
    if s.get(at) == Some(&b'*') {
        at += 1;
        i = Some(0);
    }
    let mut flag = false;
    for &c in &s[at..] {
        if c.is_ascii_digit() {
            if !flag {
                i = Some(i.map_or(0, |n| n + 1));
                flag = true;
            }
            if let Some(p) = i.and_then(|n| pos.get_mut(n)) {
                add_digit(p, i32::from(c - b'0'))?;
            }
        } else if c == b'*' {
            return Err(soap_error(
                "Encoding: '*' may only be first arraySize value in list",
            ));
        } else {
            flag = false;
        }
    }
    Ok(pos)
}

/// `calc_dimension`: the commas before `]`, and one.
fn calc_dimension(s: &[u8]) -> usize {
    1 + s
        .iter()
        .take_while(|&&c| c != b']' && c != 0)
        .filter(|&&c| c == b',')
        .count()
}

/// `get_position_ex`.
fn get_position_ex(s: &[u8], pos: &mut [i32]) -> Res<()> {
    pos.fill(0);
    let mut i = 0;
    for &c in s {
        if c == b']' || c == 0 || i >= pos.len() {
            break;
        }
        if c.is_ascii_digit() {
            add_digit(&mut pos[i], i32::from(c - b'0'))?;
        } else if c == b',' {
            i += 1;
        }
    }
    Ok(())
}

/// `get_position`.
fn get_position(dimension: usize, s: &[u8]) -> Res<Vec<i32>> {
    let mut pos = vec![0; dimension];
    get_position_ex(s, &mut pos)?;
    Ok(pos)
}

/// `strrchr(s, '[')`: the text before the last `[` and from it on.
fn split_bracket(s: &[u8]) -> (&[u8], Option<&[u8]>) {
    match s.iter().rposition(|&c| c == b'[') {
        Some(at) => (&s[..at], Some(&s[at..])),
        None => (s, None),
    }
}

/// The extra attribute `wsdl:name` of the type's attribute `ns:name`.
fn extra<'a>(t: &'a Type, ns: &[u8], name: &[u8]) -> Option<&'a crate::model::ExtraAttribute> {
    let key = [ns, b":", name].concat();
    let wkey = [WSDL_NAMESPACE, b":", name].concat();
    t.attributes
        .as_ref()?
        .get_str(&key)?
        .extra_attributes
        .as_ref()?
        .get_str(&wkey)
}

/// The type's one element with an encoder, where it has exactly one.
fn element_of<'a>(sdl: &'a Sdl, t: &Type) -> Option<&'a Type> {
    single_element(t)
        .and_then(|id| sdl.ty(id))
        .filter(|el| el.encode.is_some())
}

/// The type's one element, where it has exactly one.
fn single_element(t: &Type) -> Option<TypeId> {
    let els = t.elements.as_ref()?;
    (els.len() == 1).then(|| els.first().copied()).flatten()
}

/// `n` as php's `int` prints it.
fn dims_text(dims: &[i32], sep: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, d) in dims.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.extend_from_slice(d.to_string().as_bytes());
    }
    out
}

/// `zend_hash_num_elements` as php's `int`.
fn count(a: PhpArray) -> i32 {
    i32::try_from(a.len()).unwrap_or(i32::MAX)
}

// ---- writing

/// `add_xml_array_elements`.
#[allow(clippy::too_many_arguments)]
fn add_xml_array_elements(
    req: &mut Request,
    cx: &mut Cx,
    x_param: Node,
    type_: Option<&Type>,
    e: Option<&Enc>,
    ns: Option<Ns>,
    dims: &[i32],
    data: Option<Value>,
    style: Use,
) -> Res<()> {
    let Some(&first) = dims.first() else {
        return Ok(());
    };
    let dimension = dims.len();
    let name_it = |xparam: Option<Node>| {
        let Some(xparam) = xparam else {
            return;
        };
        if let Some(t) = type_ {
            xparam.set_name(&t.name.clone().unwrap_or_default());
        } else if style == Use::Literal
            && let Some(e) = e
            && let Some(ts) = &e.e.details.type_str
        {
            xparam.set_name(ts);
            xparam.set_ns_call(ns);
        } else {
            xparam.set_name(b"item");
        }
    };
    let empty = |req: &mut Request, cx: &mut Cx| -> Res<()> {
        if dimension == 1 {
            let xparam = new_node(x_param.doc(), b"BOGUS")?;
            x_param.add_child(xparam);
            name_it(Some(xparam));
            Ok(())
        } else {
            add_xml_array_elements(req, cx, x_param, type_, e, ns, &dims[1..], None, style)
        }
    };
    let Some(arr) = data.and_then(|d| d.as_array()) else {
        for _ in 0..first {
            empty(req, cx)?;
        }
        return Ok(());
    };
    if arr.is_recursive() {
        return Err(value_error(req, "Recursive array cannot be encoded"));
    }
    arr.protect();
    let read = rows(req, arr);
    let out = with_rows(req, &read, |req, rows| {
        let mut j = 0;
        for (_, zdata) in rows {
            if j >= first {
                break;
            }
            let zdata = *zdata;
            if dimension == 1 {
                let r = match e {
                    None => get_conversion(type_code(&zdata.deref()))?,
                    Some(e) => e.r,
                };
                let xparam = master_to_xml(req, cx, Some(r), zdata, style, x_param)?;
                name_it(xparam);
            } else {
                add_xml_array_elements(
                    req,
                    cx,
                    x_param,
                    type_,
                    e,
                    ns,
                    &dims[1..],
                    Some(zdata.deref()),
                    style,
                )?;
            }
            j += 1;
        }
        while j < first {
            empty(req, cx)?;
            j += 1;
        }
        Ok(())
    });
    arr.unprotect();
    out
}

/// `to_xml_array`'s copy of a `Traversable`'s elements, the caller's.
fn traversable_copy(req: &mut Request, o: php_ext::values::PhpObject) -> Res<Owner<PhpArray>> {
    let mut copy = PhpArray::new(req)?;
    let fill = |req: &mut Request, copy: &mut Owner<PhpArray>| -> Res<()> {
        let held = iterate(req, &o)?;
        let it = *held;
        let walk = |req: &mut Request, copy: &mut Owner<PhpArray>| -> Res<()> {
            it.rewind(req)?;
            while it.valid(req)? {
                let val = it.value(req)?;
                let key = match it.key(req) {
                    Ok(k) => k,
                    Err(t) => {
                        val.give_up(req);
                        return Err(t.into());
                    }
                };
                let k = key_of(req, &key);
                key.give_up(req);
                match k {
                    Ok(k) => copy.set(req, k, val)?,
                    Err(t) => {
                        val.give_up(req);
                        return Err(t.into());
                    }
                }
                it.next(req)?;
            }
            Ok(())
        };
        let r = walk(req, copy);
        held.give_up(req);
        r
    };
    match fill(req, &mut copy) {
        Ok(()) => Ok(copy),
        Err(err) => {
            copy.give_up(req);
            Err(err)
        }
    }
}

/// `to_xml_array`.
pub(super) fn to_xml_array(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    let soap_version = version(req);
    let x_param = new_node(parent.doc(), b"BOGUS")?;
    parent.add_child(x_param);
    let xsi_array_type = |req: &mut Request| {
        let ns = if soap_version == SOAP_1_1 {
            SOAP_1_1_ENC_NAMESPACE
        } else {
            SOAP_1_2_ENC_NAMESPACE
        };
        if super::features(req) & SOAP_USE_XSI_ARRAY_TYPE != 0 {
            set_ns_and_type_ex(req, x_param, Some(ns), b"Array");
        } else {
            set_ns_and_type(req, x_param, &e.e);
        }
    };
    if super::is_null(&data) {
        if style == Use::Encoded {
            super::set_xsi_nil(req, x_param);
            xsi_array_type(req);
        }
        return Ok(x_param);
    }
    let mut array_copy = None;
    if let ValueRef::Object(o) = data.as_ref()
        && let Some(trav) = lookup_class(req, b"Traversable")?
        && o.instance_of(req, trav)
    {
        array_copy = Some(traversable_copy(req, o)?);
    }
    let data = array_copy.as_ref().map_or(data, |c| Value::array(**c));
    let out = array_body(req, cx, e, data, style, x_param, soap_version);
    if let Some(c) = array_copy {
        c.give_up(req);
    }
    out?;
    if style == Use::Encoded {
        xsi_array_type(req);
    }
    Ok(x_param)
}

/// `to_xml_array` over an array: its type and size, then its elements.
#[allow(clippy::too_many_lines)]
fn array_body(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    x_param: Node,
    soap_version: i64,
) -> Res<()> {
    let Some(arr) = data.as_array() else {
        return Ok(());
    };
    let sdl = sdl(req);
    let sdl_ref = sdl.as_deref();
    let t = sdl_type(sdl.as_ref(), &e.e);
    let i = count(arr);
    let mut array_type: Vec<u8> = Vec::new();
    let array_size: Vec<u8>;
    let mut element_type: Option<&Type> = None;
    let mut found: Option<EncRef> = None;
    let dims: Vec<i32>;
    if let Some(t) = t
        && let Some(ext) = extra(t, SOAP_1_1_ENC_NAMESPACE, b"arrayType")
    {
        let value = ext.val.clone().unwrap_or_default();
        let (value, end) = split_bracket(&value);
        let dimension = end.map_or(1, |end| calc_dimension(&end[1..]));
        match &ext.ns {
            Some(ns) => {
                found = get_encoder(sdl_ref, Some(ns), value);
                array_type = get_type_str(req, x_param, Some(ns), value);
            }
            None => array_type.extend_from_slice(value),
        }
        let mut d = vec![0; dimension];
        d[0] = i;
        let mut el = Some(data);
        for slot in d.iter_mut().skip(1) {
            if let Some(a) = el.and_then(|v| v.as_array())
                && !a.is_empty()
            {
                let first = walk::rows(req, a).next().map(|(_, v)| v.deref());
                el = first;
                *slot = first.and_then(|v| v.as_array()).map_or(0, count);
            }
        }
        array_size = dims_text(&d, b',');
        dims = d;
    } else if let Some(t) = t
        && let Some(ext) = extra(t, SOAP_1_2_ENC_NAMESPACE, b"itemType")
    {
        let val = ext.val.clone().unwrap_or_default();
        match &ext.ns {
            Some(ns) => {
                found = get_encoder(sdl_ref, Some(ns), &val);
                array_type = get_type_str(req, x_param, Some(ns), &val);
            }
            None => array_type.extend_from_slice(&val),
        }
        if let Some(ext) = extra(t, SOAP_1_2_ENC_NAMESPACE, b"arraySize") {
            let v = ext.val.clone().unwrap_or_default();
            let mut d = get_position_12(calc_dimension_12(&v)?, &v)?;
            if let Some(first) = d.first_mut()
                && *first == 0
            {
                *first = i;
            }
            array_size = dims_text(&d, b',');
            dims = d;
        } else {
            dims = vec![0];
            array_size = i.to_string().into_bytes();
        }
    } else if let Some(t) = t
        && let Some(ext) = extra(t, SOAP_1_2_ENC_NAMESPACE, b"arraySize")
    {
        let v = ext.val.clone().unwrap_or_default();
        let mut d = get_position_12(calc_dimension_12(&v)?, &v)?;
        if let Some(first) = d.first_mut()
            && *first == 0
        {
            *first = i;
        }
        array_size = dims_text(&d, b',');
        dims = d;
        let el = sdl_ref.and_then(|s| element_of(s, t));
        let el_enc = match el.and_then(|el| el.encode) {
            Some(r) => Some(enc(req, r)?),
            None => None,
        };
        if let (Some(el), Some(ee)) = (el, el_enc.filter(|ee| ee.e.details.type_str.is_some())) {
            element_type = Some(el);
            found = Some(ee.r);
            let ts = ee.e.details.type_str.clone().unwrap_or_default();
            array_type = get_type_str(req, x_param, ee.e.details.ns.as_deref(), &ts);
        } else {
            found = get_array_type(req, x_param, &data, &mut array_type)?;
        }
    } else {
        let el = match (sdl_ref, t) {
            (Some(s), Some(t)) => element_of(s, t),
            _ => None,
        };
        let el_enc = match el.and_then(|el| el.encode) {
            Some(r) => Some(enc(req, r)?),
            None => None,
        };
        if let (Some(el), Some(ee)) = (el, el_enc.filter(|ee| ee.e.details.type_str.is_some())) {
            element_type = Some(el);
            found = Some(ee.r);
            let ts = ee.e.details.type_str.clone().unwrap_or_default();
            array_type = get_type_str(req, x_param, ee.e.details.ns.as_deref(), &ts);
        } else {
            found = get_array_type(req, x_param, &data, &mut array_type)?;
        }
        array_size = i.to_string().into_bytes();
        dims = vec![i];
    }
    if style == Use::Encoded {
        if soap_version == SOAP_1_1 {
            if array_type.as_slice() == b"xsd:anyType" {
                array_type = b"xsd:ur-type".to_vec();
            }
            array_type.push(b'[');
            array_type.extend_from_slice(&array_size);
            array_type.push(b']');
            set_ns_prop(
                req,
                x_param,
                SOAP_1_1_ENC_NAMESPACE,
                b"arrayType",
                &array_type,
            );
        } else {
            let size: Vec<u8> = array_size
                .iter()
                .map(|&c| if c == b',' { b' ' } else { c })
                .collect();
            set_ns_prop(
                req,
                x_param,
                SOAP_1_2_ENC_NAMESPACE,
                b"itemType",
                &array_type,
            );
            set_ns_prop(req, x_param, SOAP_1_2_ENC_NAMESPACE, b"arraySize", &size);
        }
    }
    let found = match found {
        Some(r) => Some(enc(req, r)?),
        None => None,
    };
    let ns = match found.as_ref().and_then(|f| f.e.details.ns.clone()) {
        Some(ns) => encode_add_ns(req, x_param, &ns),
        None => None,
    };
    add_xml_array_elements(
        req,
        cx,
        x_param,
        element_type,
        found.as_ref(),
        ns,
        &dims,
        Some(data),
        style,
    )
}

/// `get_array_type`: the one type every element has, written as
/// `prefix:type` into `out`, or `xsd:anyType`.
fn get_array_type(
    req: &mut Request,
    node: Node,
    array: &Value,
    out: &mut Vec<u8>,
) -> Res<Option<EncRef>> {
    let any_type = |out: &mut Vec<u8>| -> Res<Option<EncRef>> {
        out.extend_from_slice(b"xsd:anyType");
        Ok(Some(get_conversion(ty::XSD_ANYTYPE)?))
    };
    let Some(arr) = array.as_array() else {
        return any_type(out);
    };
    let soap_var = lookup_class(req, b"SoapVar")?;
    let mut i = 0;
    let mut different = false;
    let (mut cur_type, mut prev_type) = (0, 0);
    let (mut cur_stype, mut prev_stype): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let (mut cur_ns, mut prev_ns): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    for (_, tmp) in walk::rows(req, arr) {
        let tmp = tmp.deref();
        match tmp.as_ref() {
            ValueRef::Object(o) if Some(o.class()) == soap_var => {
                let slot = |n| crate::shared::slot(o, n);
                let Some(t) = slot(VAR_ENC_TYPE).as_int() else {
                    return Err(soap_error("Encoding: SoapVar has no 'enc_type' property"));
                };
                cur_type = as_int(t);
                if cur_type == ty::UNKNOWN_TYPE {
                    cur_type = type_code(&slot(VAR_ENC_VALUE));
                }
                cur_stype = slot(VAR_ENC_STYPE).as_str_bytes().map(<[u8]>::to_vec);
                cur_ns = slot(VAR_ENC_NS).as_str_bytes().map(<[u8]>::to_vec);
            }
            ValueRef::Array(a) if !a.is_list() => {
                cur_type = ty::APACHE_MAP;
                cur_stype = None;
                cur_ns = None;
            }
            _ => {
                cur_type = type_code(&tmp);
                cur_stype = None;
                cur_ns = None;
            }
        }
        if i > 0
            && (cur_type != prev_type
                || matches!((&cur_stype, &prev_stype), (Some(a), Some(b)) if a != b)
                || (cur_stype.is_none() && prev_stype.is_some())
                || matches!((&cur_ns, &prev_ns), (Some(a), Some(b)) if a != b)
                || (cur_ns.is_none() && prev_ns.is_some()))
        {
            different = true;
            break;
        }
        prev_type = cur_type;
        prev_stype.clone_from(&cur_stype);
        prev_ns.clone_from(&cur_ns);
        i += 1;
    }
    if different || i == 0 {
        return any_type(out);
    }
    if let Some(stype) = cur_stype {
        let mut array_type = Vec::new();
        if let Some(ns) = &cur_ns {
            let prefix = encode_add_ns(req, node, ns).and_then(Ns::prefix);
            out.extend_from_slice(&prefix.unwrap_or_default());
            out.push(b':');
            array_type.extend_from_slice(ns);
            array_type.push(b':');
        }
        out.extend_from_slice(&stype);
        array_type.extend_from_slice(&stype);
        let s = sdl(req);
        return Ok(get_encoder_ex(s.as_deref(), &array_type));
    }
    let found = conv(req, cur_type)?;
    let ts = found.e.details.type_str.clone().unwrap_or_default();
    let t = get_type_str(req, node, found.e.details.ns.as_deref(), &ts);
    out.extend_from_slice(&t);
    Ok(Some(found.r))
}

// ---- reading

/// The type and dimensions an encoded array names by its attributes, or
/// its WSDL type by its own.
struct Shape {
    found: Option<EncRef>,
    dims: Option<Vec<i32>>,
}

/// `to_zval_array`'s reading of the attributes and the WSDL type.
fn array_shape(req: &mut Request, e: &Enc, data: Node) -> Res<Shape> {
    let sdl = sdl(req);
    let sdl_ref = sdl.as_deref();
    let props = data.properties();
    let content_of = |a: Option<Node>| {
        a.map(attr_content)
            .filter(|_| a.and_then(Node::children).is_some())
    };
    if let Some(content) = content_of(get_soap_enc_attribute(props, b"arrayType")) {
        let attr = get_soap_enc_attribute(props, b"arrayType");
        let (type_, ns) = parse_namespace(&content);
        let nsptr = attr.and_then(Node::parent).and_then(|p| p.search_ns(ns));
        let (type_dup, end) = split_bracket(type_);
        let mut dims = None;
        if let Some(end) = end {
            let dimension = calc_dimension(&end[1..]);
            dims = Some(get_position(dimension, &end[1..])?);
        }
        let found = nsptr.and_then(|n| get_encoder(sdl_ref, Some(&crate::xml::href(n)), type_dup));
        return Ok(Shape { found, dims });
    }
    if let Some(content) = content_of(get_soap_enc_attribute(props, b"itemType")) {
        let attr = get_soap_enc_attribute(props, b"itemType");
        let (type_, ns) = parse_namespace(&content);
        let nsptr = attr.and_then(Node::parent).and_then(|p| p.search_ns(ns));
        let found = nsptr.and_then(|n| get_encoder(sdl_ref, Some(&crate::xml::href(n)), type_));
        let dims = match content_of(get_soap_enc_attribute(props, b"arraySize")) {
            Some(size) => get_position_12(calc_dimension_12(&size)?, &size)?,
            None => vec![0],
        };
        return Ok(Shape {
            found,
            dims: Some(dims),
        });
    }
    if let Some(size) = content_of(get_soap_enc_attribute(props, b"arraySize")) {
        let dims = get_position_12(calc_dimension_12(&size)?, &size)?;
        return Ok(Shape {
            found: None,
            dims: Some(dims),
        });
    }
    let Some(t) = sdl_type(sdl.as_ref(), &e.e) else {
        return Ok(Shape {
            found: None,
            dims: None,
        });
    };
    if let Some(ext) = extra(t, SOAP_1_1_ENC_NAMESPACE, b"arrayType") {
        let val = ext.val.clone().unwrap_or_default();
        let (type_, _) = split_bracket(&val);
        let found = ext
            .ns
            .as_deref()
            .and_then(|ns| get_encoder(sdl_ref, Some(ns), type_));
        return Ok(Shape {
            found,
            dims: Some(vec![0]),
        });
    }
    if let Some(ext) = extra(t, SOAP_1_2_ENC_NAMESPACE, b"itemType") {
        let val = ext.val.clone().unwrap_or_default();
        let found = ext
            .ns
            .as_deref()
            .and_then(|ns| get_encoder(sdl_ref, Some(ns), &val));
        let dims = match extra(t, SOAP_1_2_ENC_NAMESPACE, b"arraySize") {
            Some(ext) => {
                let v = ext.val.clone().unwrap_or_default();
                get_position_12(calc_dimension_12(&v)?, &v)?
            }
            None => vec![0],
        };
        return Ok(Shape {
            found,
            dims: Some(dims),
        });
    }
    let element_enc = |t: &Type| {
        single_element(t)
            .and_then(|id| sdl_ref?.ty(id))
            .and_then(|el| el.encode)
    };
    if let Some(ext) = extra(t, SOAP_1_2_ENC_NAMESPACE, b"arraySize") {
        let v = ext.val.clone().unwrap_or_default();
        let dims = get_position_12(calc_dimension_12(&v)?, &v)?;
        return Ok(Shape {
            found: element_enc(t),
            dims: Some(dims),
        });
    }
    Ok(Shape {
        found: element_enc(t),
        dims: None,
    })
}

/// `to_zval_array`.
pub(super) fn to_zval_array(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Node,
) -> Res<Owner<Value>> {
    if xml_nil(data) {
        return Ok(Owner::null());
    }
    let shape = array_shape(req, e, data)?;
    let dims = match shape.dims {
        Some(d) if !d.is_empty() => d,
        _ => vec![0],
    };
    let dimension = dims.len();
    let mut pos = vec![0i32; dimension];
    if let Some(attr) = get_soap_enc_attribute(data.properties(), b"offset")
        && attr.children().is_some()
    {
        let content = attr_content(attr);
        let (_, end) = split_bracket(&content);
        get_position_ex(end.unwrap_or(&content), &mut pos)?;
    }
    let ret = PhpArray::new(req)?;
    match fill_array(req, cx, *ret, shape.found, data, &dims, &mut pos) {
        Ok(true) => Ok(ret.into_value()),
        // `ZVAL_NULL(ret)` once the memory limit was met, since 8.5.12.
        Ok(false) => {
            ret.give_up(req);
            Ok(Owner::null())
        }
        Err(err) => {
            ret.give_up(req);
            Err(err)
        }
    }
}

/// `to_zval_array`'s walk of the elements, each placed at its position;
/// `false` where, since 8.5.12, the memory limit was met making the
/// intermediate arrays of a multidimensional one.
fn fill_array(
    req: &mut Request,
    cx: &mut Cx,
    ret: PhpArray,
    found: Option<EncRef>,
    data: Node,
    dims: &[i32],
    pos: &mut [i32],
) -> Res<bool> {
    let dimension = dims.len();
    for trav in crate::xml::children(data) {
        if trav.kind() != Kind::Element {
            continue;
        }
        let position = get_soap_enc_attribute(trav.properties(), b"position");
        let tmp = master_to_zval(req, cx, found, trav)?;
        if let Some(p) = position
            && p.children().is_some()
        {
            let content = attr_content(p);
            let (_, end) = split_bracket(&content);
            if let Err(err) = get_position_ex(end.unwrap_or(&content), pos) {
                tmp.give_up(req);
                return Err(err);
            }
        }
        let mut ar = ret;
        for p in &pos[..dimension - 1] {
            let k = Key::Int(i64::from(*p));
            match ar.get(&k).and_then(|v| v.as_array()) {
                Some(a2) => ar = a2,
                None => {
                    let fresh = PhpArray::new(req)?;
                    let view = *fresh;
                    ar.set(req, Owner::take(req, k), fresh.into_value())?;
                    ar = view;
                }
            }
            if req.memory().has_oom() {
                tmp.give_up(req);
                return Ok(false);
            }
        }
        ar.set(req, Owner::index(i64::from(pos[dimension - 1])), tmp)?;
        let mut i = dimension;
        while i > 0 {
            i -= 1;
            if pos[i] == i32::MAX {
                return Err(soap_error("Encoding: array index out of range"));
            }
            pos[i] += 1;
            if pos[i] < dims[i] {
                break;
            }
            if i > 0 {
                pos[i] = 0;
            }
        }
    }
    Ok(true)
}

// ---- the Apache map

/// `to_xml_map`.
pub(super) fn to_xml_map(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let x_param = new_node(parent.doc(), b"BOGUS")?;
    parent.add_child(x_param);
    if find_zval_null(req, &data, x_param, style) {
        return Ok(Some(x_param));
    }
    if let Some(arr) = data.as_array() {
        if arr.is_recursive() {
            return Err(value_error(req, "Recursive array cannot be encoded"));
        }
        arr.protect();
        let read = rows(req, arr);
        let out = with_rows(req, &read, |req, rows| {
            for (k, temp) in rows {
                let item = new_node(parent.doc(), b"item")?;
                x_param.add_child(item);
                let key = new_node(parent.doc(), b"key")?;
                item.add_child(key);
                match k {
                    K::Str(s) => {
                        if style == Use::Encoded {
                            set_xsi_type(req, key, b"xsd:string");
                        }
                        key.set_content(s);
                    }
                    K::Int(n) => {
                        if style == Use::Encoded {
                            set_xsi_type(req, key, b"xsd:int");
                        }
                        key.set_content_len(n.to_string().as_bytes());
                    }
                }
                let conv = get_conversion(type_code(&temp.deref()))?;
                let xparam = master_to_xml(req, cx, Some(conv), *temp, style, item)?;
                if let Some(x) = xparam {
                    x.set_name(b"value");
                }
            }
            Ok(())
        });
        arr.unprotect();
        out?;
    }
    if style == Use::Encoded {
        set_ns_and_type(req, x_param, &e.e);
    }
    Ok(Some(x_param))
}

/// `to_zval_map`.
pub(super) fn to_zval_map(req: &mut Request, cx: &mut Cx, data: Node) -> Res<Owner<Value>> {
    if xml_nil(data) || data.children().is_none() {
        return Ok(Owner::null());
    }
    let ret = PhpArray::new(req)?;
    match fill_map(req, cx, *ret, data) {
        Ok(()) => Ok(ret.into_value()),
        Err(err) => {
            ret.give_up(req);
            Err(err)
        }
    }
}

/// `to_zval_map`'s `item`s, each a key and a value.
fn fill_map(req: &mut Request, cx: &mut Cx, mut ret: PhpArray, data: Node) -> Res<()> {
    let mut trav = data.children();
    while let Some(n) = trav {
        let Some(item) = get_node(Some(n), b"item", None) else {
            break;
        };
        let Some(xml_key) = get_node(item.children(), b"key", None) else {
            return Err(soap_error("Encoding: Can't decode apache map, missing key"));
        };
        let Some(xml_value) = get_node(item.children(), b"value", None) else {
            return Err(soap_error(
                "Encoding: Can't decode apache map, missing value",
            ));
        };
        let key = master_to_zval(req, cx, None, xml_key)?;
        let value = match master_to_zval(req, cx, None, xml_value) {
            Ok(v) => v,
            Err(err) => {
                key.give_up(req);
                return Err(err);
            }
        };
        let k = match key.as_ref() {
            // `zend_symtable_update` under the string the key decoded to.
            ValueRef::Str(s) => Some(Owner::take(req, Key::lookup(s))),
            ValueRef::Int(n) => Some(Owner::index(n)),
            _ => None,
        };
        key.give_up(req);
        match k {
            Some(k) => ret.set(req, k, value)?,
            None => {
                value.give_up(req);
                return Err(soap_error(
                    "Encoding: Can't decode apache map, only Strings or Longs are allowed as keys",
                ));
            }
        }
        trav = item.next();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{calc_dimension, calc_dimension_12, get_position, get_position_12};

    #[test]
    fn dimensions_as_php() {
        assert_eq!(calc_dimension(b"3,4]"), 2);
        assert_eq!(calc_dimension(b"]"), 1);
        assert_eq!(calc_dimension_12(b"* 3").ok(), Some(2));
        assert_eq!(calc_dimension_12(b"2 3").ok(), Some(2));
        assert!(calc_dimension_12(b"2 *").is_err());
        assert_eq!(get_position(2, b"1,2]").ok(), Some(vec![1, 2]));
        assert_eq!(get_position_12(2, b"* 5").ok(), Some(vec![0, 5]));
        assert_eq!(get_position_12(1, b"12").ok(), Some(vec![12]));
    }
}

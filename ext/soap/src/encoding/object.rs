//! `php_encoding.c`'s structs: an object or an array to an element whose
//! children are its properties, by the WSDL type's content model where
//! there is one, and back.

use std::sync::Arc;

use php_ext::call::{autoload, new_object};
use php_ext::raising::throw_error;
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{ClassId, Key, Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_libxml::ffi::{Kind, Node};

use super::array::{K, rows, with_rows};
use super::{
    Cx, Enc, chain, child_content, class, enc, encode_add_ns, get_conversion, master_to_xml,
    master_to_zval, master_to_zval_int, new_node, sdl_type, set_ns_and_type, set_xsi_nil,
    soap_add_xml_ref, soap_check_xml_ref, soap_check_zval_ref, text, type_code, xml_nil,
};
use crate::model::{
    Content, ContentKind, ContentModel, EncRef, Form, Sdl, ToZval, TypeId, TypeKind, Use,
    XML_NAMESPACE, ty,
};
use crate::xml::{get_attribute_any_ns, get_node};
use crate::{Res, State, soap_error};

/// `SOAP_SINGLE_ELEMENT_ARRAYS`.
const SOAP_SINGLE_ELEMENT_ARRAYS: i64 = 1;

// ---- properties

/// `get_zval_property`: a view of the property or the element, none where
/// there is none.
pub(super) fn get_zval_property(req: &mut Request, object: &Value, name: &[u8]) -> Option<Value> {
    match object.as_ref() {
        ValueRef::Object(o) => {
            if let Some(v) = o.prop(req, name) {
                return Some(v.deref());
            }
            // `zend_read_property(.., silent)`: the class's handler, so a
            // `__get` answers; the value it made is held until the
            // serialisation ends (`encode_finish`), as php's `rv` is.
            let answered = o
                .handlers(req)
                .read_property(req, o, name, php_ext::abi::PropertyRead::Silent)
                .ok()?;
            // The value a reference answered names, held in its place.
            let read = Owner::take(req, answered.deref());
            answered.give_up(req);
            if matches!(read.as_ref(), ValueRef::Null | ValueRef::Undef) {
                read.give_up(req);
                return None;
            }
            let view = *read;
            req.state::<crate::State>().held.push(read);
            Some(view)
        }
        ValueRef::Array(a) => a.find(name).map(|v| v.deref()),
        _ => None,
    }
}

/// `set_zval_property`: `val` is the property's from here.
fn set_zval_property(req: &mut Request, object: &Value, name: &[u8], val: Owner<Value>) -> Res<()> {
    let Some(o) = object.as_object() else {
        val.give_up(req);
        return Ok(());
    };
    let name = match PhpString::new(req, name) {
        Ok(n) => n,
        Err(t) => {
            val.give_up(req);
            return Err(t.into());
        }
    };
    Ok(o.set_prop(req, name, val)?)
}

/// `unset_zval_property`.
fn unset_zval_property(req: &mut Request, object: &Value, name: &[u8]) -> Res<()> {
    match object.as_ref() {
        ValueRef::Object(o) => {
            let s = PhpString::new(req, name)?;
            let r = o.unset_row(req, &Key::Str(*s));
            s.give_up(req);
            r?;
        }
        ValueRef::Array(a) => {
            let mut a = a;
            let s = PhpString::new(req, name)?;
            let r = a.unset(req, &Key::Str(*s));
            s.give_up(req);
            r?;
        }
        _ => {}
    }
    Ok(())
}

/// `add_assoc_zval`: `val` is the array's from here.
fn add_assoc(req: &mut Request, arr: &mut PhpArray, name: &[u8], val: Owner<Value>) -> Res<()> {
    Ok(arr.set_symtable_str(req, name, val)?)
}

/// `array_init` with one element, `val`.
fn array_of(req: &mut Request, val: Owner<Value>) -> Res<Owner<PhpArray>> {
    let mut arr = match PhpArray::new(req) {
        Ok(arr) => arr,
        Err(thrown) => {
            val.give_up(req);
            return Err(thrown.into());
        }
    };
    arr.append(req, val)?;
    Ok(arr)
}

/// A property of an object holding an array, `val` added at its end
/// (`SEPARATE_ARRAY` and `add_next_index_zval`): in place where the
/// property's is the array's one owner, and otherwise to a copy, made
/// through an owner of the array taken for the write and written back to
/// the property, which gives up the property's owner of the table it
/// shared with the class's default or whoever else holds it.
fn append_to_prop(
    req: &mut Request,
    ret: &Value,
    name: &[u8],
    arr: PhpArray,
    val: Owner<Value>,
) -> Res<()> {
    let shared = Value::array(arr);
    if shared.is_refcounted() && shared.refcount() == 1 {
        let mut a = arr;
        a.append(req, val)?;
        return Ok(());
    }
    let mut copy = Owner::take(req, arr);
    if let Err(thrown) = copy.append(req, val) {
        copy.give_up(req);
        return Err(thrown.into());
    }
    set_zval_property(req, ret, name, copy.into_value())?;
    Ok(())
}

// ---- reading

/// `model_to_zval_any`.
fn model_to_zval_any(
    req: &mut Request,
    cx: &mut Cx,
    ret: &Value,
    mut node: Option<Node>,
) -> Res<()> {
    let anyxml = get_conversion(ty::XSD_ANYXML)?;
    let mut any: Option<Owner<Value>> = None;
    let mut name: Option<Vec<u8>> = None;
    while let Some(mut n) = node {
        if get_zval_property(req, ret, &n.name()).is_none() {
            let stepped = any_step(req, cx, anyxml, n, &mut any, &mut name);
            n = match stepped {
                Ok(n) => n,
                Err(err) => {
                    if let Some(a) = any.take() {
                        a.give_up(req);
                    }
                    return Err(err);
                }
            };
        }
        node = n.next();
    }
    if let Some(a) = any {
        let key = name.unwrap_or_else(|| b"any".to_vec());
        set_zval_property(req, ret, &key, a)?;
    }
    Ok(())
}

/// One element `model_to_zval_any` reads into what it gathers, and the
/// last node it read, the XML text of consecutive elements joined into
/// one value. What is gathered stays the caller's to give up where this
/// fails.
fn any_step(
    req: &mut Request,
    cx: &mut Cx,
    anyxml: EncRef,
    mut n: Node,
    any: &mut Option<Owner<Value>>,
    name: &mut Option<Vec<u8>>,
) -> Res<Node> {
    let mut val = master_to_zval(req, cx, Some(anyxml), n)?;
    if any.as_ref().is_some_and(|a| a.as_array().is_none())
        && let Some(a) = any.take()
    {
        let mut arr = PhpArray::new(req)?;
        match name.as_deref() {
            Some(nm) => {
                arr.set_symtable_str(req, nm, a)?;
            }
            None => arr.append(req, a)?,
        }
        *any = Some(arr.into_value());
    }
    if val.as_str_bytes().is_some_and(|s| s.first() == Some(&b'<')) {
        *name = None;
        while let Some(next) = n.next() {
            let val2 = match master_to_zval(req, cx, Some(anyxml), next) {
                Ok(val2) => val2,
                Err(err) => {
                    val.give_up(req);
                    return Err(err);
                }
            };
            let Some(tail) = val2.as_str_bytes().map(<[u8]>::to_vec) else {
                val2.give_up(req);
                break;
            };
            let mut joined = val.as_str_bytes().unwrap_or_default().to_vec();
            joined.extend_from_slice(&tail);
            let fresh = super::string(req, &joined)?;
            val.give_up(req);
            val2.give_up(req);
            val = fresh;
            n = next;
        }
    } else {
        *name = Some(n.name());
    }
    match any.take() {
        None => {
            if let Some(nm) = name.take() {
                let mut arr = PhpArray::new(req)?;
                arr.set_symtable_str(req, &nm, val)?;
                *any = Some(arr.into_value());
            } else {
                *any = Some(val);
            }
        }
        Some(a) => {
            let mut arr = a.as_array().unwrap_or_else(PhpArray::empty);
            if let Some(nm) = name.as_deref() {
                match arr.find(nm) {
                    Some(el) if el.as_array().is_some() => {
                        let mut inner = el.as_array().unwrap_or_else(PhpArray::empty);
                        inner.append(req, val)?;
                    }
                    // php puts the old element and this one in an
                    // array it keeps nowhere: the value is lost.
                    Some(_) => val.give_up(req),
                    None => add_assoc(req, &mut arr, nm, val)?,
                }
            } else {
                arr.append(req, val)?;
            }
            // The rows went into the array `a` owns, written in
            // place: it has the one owner, so nothing separated.
            *any = Some(a);
            *name = None;
        }
    }
    Ok(n)
}

/// The value of one element of a content model, its `fixed` and
/// `default` read in where the element is empty.
fn element_value(
    req: &mut Request,
    cx: &mut Cx,
    el: &crate::model::Type,
    node: Node,
    first: bool,
) -> Res<Owner<Value>> {
    let name = el.name.clone().unwrap_or_default();
    let r_node = if first {
        super::check_and_resolve_href(node)?
    } else {
        node
    };
    if let Some(content) = child_content(r_node) {
        if let Some(fixed) = &el.fixed
            && *fixed != content
        {
            return Err(soap_error(format_args!(
                "Encoding: Element '{}' has fixed value '{}' (value '{}' is not allowed)",
                text(&name),
                text(fixed),
                text(&content)
            )));
        }
        return master_to_zval(req, cx, el.encode, r_node);
    }
    let preset = match (&el.fixed, &el.def) {
        (Some(f), _) => Some(f),
        (None, Some(d)) if !el.nillable => Some(d),
        _ => None,
    };
    let Some(preset) = preset else {
        return master_to_zval(req, cx, el.encode, r_node);
    };
    let dummy = new_node(node.doc(), b"BOGUS")?;
    dummy.set_content(preset);
    let out = master_to_zval(req, cx, el.encode, dummy);
    dummy.free();
    out
}

/// `model_to_zval_object`.
fn model_to_zval_object(
    req: &mut Request,
    cx: &mut Cx,
    ret: &Value,
    model: &ContentModel,
    data: Node,
    sdl: &Arc<Sdl>,
) -> Res<()> {
    match (&model.u, model.kind) {
        (Content::Element(id), ContentKind::Element) => {
            let Some(el) = sdl.ty(*id) else {
                return Ok(());
            };
            let Some(name) = el.name.clone() else {
                return Ok(());
            };
            let Some(node) = get_node(data.children(), &name, None) else {
                return Ok(());
            };
            let mut val = element_value(req, cx, el, node, true)?;
            let mut next = node.next().and_then(|n| get_node(Some(n), &name, None));
            if next.is_some() {
                let mut array = array_of(req, val)?;
                while let Some(n) = next {
                    let v = match element_value(req, cx, el, n, false) {
                        Ok(v) => v,
                        Err(err) => {
                            array.give_up(req);
                            return Err(err);
                        }
                    };
                    array.append(req, v)?;
                    next = n.next().and_then(|m| get_node(Some(m), &name, None));
                }
                val = array.into_value();
            } else if (!matches!(val.as_ref(), ValueRef::Null) || !el.nillable)
                && super::features(req) & SOAP_SINGLE_ELEMENT_ARRAYS != 0
                && (model.max_occurs == -1 || model.max_occurs > 1)
            {
                val = array_of(req, val)?.into_value();
            }
            set_zval_property(req, ret, &name, val)?;
        }
        (
            Content::Particles(list),
            ContentKind::All | ContentKind::Sequence | ContentKind::Choice,
        ) => {
            let mut any = false;
            for tmp in list {
                if tmp.kind == ContentKind::Any {
                    any = true;
                } else {
                    model_to_zval_object(req, cx, ret, tmp, data, sdl)?;
                }
            }
            if any {
                model_to_zval_any(req, cx, ret, data.children())?;
            }
        }
        (Content::Group(id), ContentKind::Group) => {
            if let Some(m) = sdl.ty(*id).and_then(|g| g.model.as_deref()) {
                model_to_zval_object(req, cx, ret, m, data, sdl)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `zend_fetch_class(name, ZEND_FETCH_CLASS_AUTO)`.
fn fetch_class(req: &mut Request, name: &[u8]) -> Res<ClassId> {
    match autoload(req, name)? {
        Some(c) => Ok(c),
        None => {
            Err(throw_error(req, "Error", &format!("Class \"{}\" not found", text(name))).into())
        }
    }
}

/// `object_init_ex`.
fn object_init(req: &mut Request, ce: ClassId) -> Res<Owner<Value>> {
    Ok(new_object(req, ce)?.into_value())
}

/// The encoder a restriction reads its base through: down the chain
/// while the types are not simple, a list or a union.
fn simple_base(
    req: &mut Request,
    sdl: Option<&Arc<Sdl>>,
    start: Option<EncRef>,
) -> Res<Option<EncRef>> {
    let mut e = start;
    let mut steps = 0u32;
    while let Some(r) = e {
        match chain(req, sdl, r)? {
            Some((kind, next))
                if !matches!(kind, TypeKind::Simple | TypeKind::List | TypeKind::Union) =>
            {
                e = next;
            }
            _ => break,
        }
        steps += 1;
        if steps > 10_000 {
            break;
        }
    }
    Ok(e)
}

/// `to_zval_object`.
pub(super) fn to_zval_object(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Node,
) -> Res<Owner<Value>> {
    to_zval_object_ex(req, cx, e, data, None)
}

/// `to_zval_object_ex`'s class: the one asked for, else the classmap's.
fn object_class(req: &mut Request, e: &Enc, pce: Option<ClassId>) -> Res<ClassId> {
    if let Some(c) = pce {
        return Ok(c);
    }
    let std = class(req, "stdClass")?;
    let Some(map) = req
        .state::<State>()
        .class_map
        .and_then(|m| m.deref().as_array())
    else {
        return Ok(std);
    };
    let Some(type_str) = e.e.details.type_str.as_deref() else {
        return Ok(std);
    };
    let classname =
        e.e.details
            .clark_notation
            .as_deref()
            .and_then(|c| map.find(c))
            .or_else(|| map.find(type_str))
            .map(|v| v.deref());
    match classname
        .as_ref()
        .and_then(Value::as_str_bytes)
        .map(<[u8]>::to_vec)
    {
        Some(name) => fetch_class(req, &name),
        None => Ok(std),
    }
}

/// `to_zval_object_ex`: an extension of a complex type decoded into the
/// class the classmap names for it, the base's converter handed that class.
/// Where the base is decoded by its own converter instead, a typemap's
/// `from_xml` for a base restricting `soapenc:Array` among them, the object
/// it answers takes the class, the caller's own name for it too.
///
/// ```php
/// $w = '<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:s="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:x="http://www.w3.org/2001/XMLSchema" xmlns:e="http://schemas.xmlsoap.org/soap/encoding/" xmlns:t="urn:t" targetNamespace="urn:t"><types><x:schema targetNamespace="urn:t"><x:complexType name="B"><x:complexContent><x:restriction base="e:Array"/></x:complexContent></x:complexType><x:complexType name="D"><x:complexContent><x:extension base="t:B"><x:sequence><x:element name="b" type="x:int"/></x:sequence></x:extension></x:complexContent></x:complexType></x:schema></types><message name="m"/><message name="r"><part name="o" type="t:D"/></message><portType name="P"><operation name="f"><input message="t:m"/><output message="t:r"/></operation></portType><binding name="N" type="t:P"><s:binding style="rpc" transport="http://schemas.xmlsoap.org/soap/http"/><operation name="f"><input><s:body use="literal" namespace="urn:t"/></input><output><s:body use="literal" namespace="urn:t"/></output></operation></binding><service name="S"><port name="p" binding="t:N"><s:address location="http://x/"/></port></service></definitions>';
/// #[AllowDynamicProperties] class Y {}
/// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, ?string $u = null): string { return '<E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/"><E:Body><t:fResponse xmlns:t="urn:t"><o><b>7</b></o></t:fResponse></E:Body></E:Envelope>'; } }
/// $kept = null;
/// $c = new C('data://text/plain;base64,' . base64_encode($w), ['cache_wsdl' => WSDL_CACHE_NONE, 'classmap' => ['D' => 'Y'], 'typemap' => [['type_ns' => 'urn:t', 'type_name' => 'B', 'from_xml' => function ($x) use (&$kept) { return $kept = (object)['from' => 'xml']; }]]]);
/// var_dump($c->f(), $kept instanceof Y);
/// ```
/// ```expect
/// object(Y)#4 (2) {
///   ["from"]=>
///   string(3) "xml"
///   ["b"]=>
///   int(7)
/// }
/// bool(true)
/// ```
///
/// ```php
/// $w = '<definitions xmlns="http://schemas.xmlsoap.org/wsdl/" xmlns:s="http://schemas.xmlsoap.org/wsdl/soap/" xmlns:x="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:t" targetNamespace="urn:t"><types><x:schema targetNamespace="urn:t"><x:complexType name="B"><x:sequence><x:element name="a" type="x:string"/></x:sequence></x:complexType><x:complexType name="D"><x:complexContent><x:extension base="t:B"><x:sequence><x:element name="b" type="x:int"/></x:sequence></x:extension></x:complexContent></x:complexType></x:schema></types><message name="m"/><message name="r"><part name="o" type="t:D"/></message><portType name="P"><operation name="f"><input message="t:m"/><output message="t:r"/></operation></portType><binding name="N" type="t:P"><s:binding style="rpc" transport="http://schemas.xmlsoap.org/soap/http"/><operation name="f"><input><s:body use="literal" namespace="urn:t"/></input><output><s:body use="literal" namespace="urn:t"/></output></operation></binding><service name="S"><port name="p" binding="t:N"><s:address location="http://x/"/></port></service></definitions>';
/// class Y { public $b; public $a; }
/// class C extends SoapClient { function __doRequest($r, $l, $a, $v, $o = false, ?string $u = null): string { return '<E:Envelope xmlns:E="http://schemas.xmlsoap.org/soap/envelope/"><E:Body><t:fResponse xmlns:t="urn:t"><o><a>A</a><b>7</b></o></t:fResponse></E:Body></E:Envelope>'; } }
/// var_dump((new C('data://text/plain;base64,' . base64_encode($w), ['cache_wsdl' => WSDL_CACHE_NONE, 'classmap' => ['D' => 'Y']]))->f());
/// ```
/// ```expect
/// object(Y)#3 (2) {
///   ["b"]=>
///   int(7)
///   ["a"]=>
///   string(1) "A"
/// }
/// ```
fn to_zval_object_ex(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Node,
    pce: Option<ClassId>,
) -> Res<Owner<Value>> {
    let ce = object_class(req, e, pce)?;
    let std = class(req, "stdClass")?;
    let sdl = super::sdl(req);
    let Some(sdl_arc) = sdl.clone() else {
        return plain_object(req, cx, data, ce);
    };
    let Some(t) = sdl_type(Some(&sdl_arc), &e.e) else {
        return plain_object(req, cx, data, ce);
    };
    let mut redo_any = false;
    let ret = if t.kind == TypeKind::Restriction
        && let Some(te) = t.encode
        && te != e.r
    {
        if let Some(base) = simple_base(req, sdl.as_ref(), Some(te))? {
            if let Some(v) = soap_check_xml_ref(req, cx, data) {
                return Ok(v);
            }
            let ret = object_init(req, ce)?;
            let filled = master_to_zval_int(req, cx, base, data)
                .and_then(|b| set_zval_property(req, &ret, b"_", b));
            kept_or_given_up(req, ret, filled)?
        } else {
            match new_object_ref(req, cx, data, ce)? {
                Fresh::Found(v) => return Ok(v),
                Fresh::Made(v) => v,
            }
        }
    } else if t.kind == TypeKind::Extension
        && let Some(te) = t.encode
        && te != e.r
    {
        let base = enc(req, te)?;
        let base_type = sdl_type(Some(&sdl_arc), &base.e);
        if base_type.is_some_and(|bt| {
            !matches!(bt.kind, TypeKind::Simple | TypeKind::List | TypeKind::Union)
        }) {
            if xml_nil(data) {
                return Ok(Owner::null());
            }
            if let Some(v) = soap_check_xml_ref(req, cx, data) {
                return Ok(v);
            }
            let direct = ce != std
                && base.e.to_zval == ToZval::SdlGuess
                && base_type.is_some_and(|bt| {
                    matches!(
                        bt.kind,
                        TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension
                    )
                })
                && !super::is_array_encoder(req, base_type.and_then(|bt| bt.encode))?;
            let ret = if direct {
                to_zval_object_ex(req, cx, &base, data, Some(ce))?
            } else {
                master_to_zval_int(req, cx, te, data)?
            };
            soap_add_xml_ref(req, cx, &ret, data);
            redo_any = get_zval_property(req, &ret, b"any").is_some();
            // `zobj->ce = ce` on what the base made, as far as
            // `PhpObject::set_class` takes it; an object it does not
            // take keeps its class here (#2399).
            if ce != std
                && let Some(o) = ret.as_object()
                && o.class() != ce
            {
                o.set_class(req, ce);
            }
            ret
        } else {
            if let Some(v) = soap_check_xml_ref(req, cx, data) {
                return Ok(v);
            }
            let ret = object_init(req, ce)?;
            soap_add_xml_ref(req, cx, &ret, data);
            let filled = master_to_zval_int(req, cx, te, data)
                .and_then(|b| set_zval_property(req, &ret, b"_", b));
            kept_or_given_up(req, ret, filled)?
        }
    } else {
        match new_object_ref(req, cx, data, ce)? {
            Fresh::Found(v) => return Ok(v),
            Fresh::Made(v) => v,
        }
    };
    let filled = fill_from_type(req, cx, &ret, t, data, &sdl_arc, redo_any);
    kept_or_given_up(req, ret, filled)
}

/// `ret` where `filled` answered, and given up where it failed.
fn kept_or_given_up(req: &mut Request, ret: Owner<Value>, filled: Res<()>) -> Res<Owner<Value>> {
    match filled {
        Ok(()) => Ok(ret),
        Err(err) => {
            ret.give_up(req);
            Err(err)
        }
    }
}

/// What `FIND_XML_NULL`, `soap_check_xml_ref` and `object_init_ex`
/// answered, each one owner, the caller's.
enum Fresh {
    Found(Owner<Value>),
    Made(Owner<Value>),
}

/// `FIND_XML_NULL`, `soap_check_xml_ref`, `object_init_ex` and `soap_add_xml_ref`.
fn new_object_ref(req: &mut Request, cx: &mut Cx, data: Node, ce: ClassId) -> Res<Fresh> {
    if xml_nil(data) {
        return Ok(Fresh::Found(Owner::null()));
    }
    if let Some(v) = soap_check_xml_ref(req, cx, data) {
        return Ok(Fresh::Found(v));
    }
    let ret = object_init(req, ce)?;
    soap_add_xml_ref(req, cx, &ret, data);
    Ok(Fresh::Made(ret))
}

/// `to_zval_object_ex`'s model and attributes of a WSDL type.
fn fill_from_type(
    req: &mut Request,
    cx: &mut Cx,
    ret: &Value,
    t: &crate::model::Type,
    data: Node,
    sdl: &Arc<Sdl>,
    redo_any: bool,
) -> Res<()> {
    if let Some(model) = t.model.as_deref() {
        model_to_zval_object(req, cx, ret, model, data, sdl)?;
        if redo_any {
            if get_zval_property(req, ret, b"any").is_none() {
                model_to_zval_any(req, cx, ret, data.children())?;
                soap_add_xml_ref(req, cx, ret, data);
            } else {
                unset_zval_property(req, ret, b"any")?;
            }
        }
    }
    let Some(attrs) = &t.attributes else {
        return Ok(());
    };
    for attr in attrs.values() {
        let Some(name) = &attr.name else {
            continue;
        };
        let val = get_attribute_any_ns(data.properties(), name);
        let str_val = match val.and_then(child_content) {
            Some(s) => {
                if let Some(fixed) = &attr.fixed
                    && *fixed != s
                {
                    return Err(soap_error(format_args!(
                        "Encoding: Attribute '{}' has fixed value '{}' (value '{}' is not allowed)",
                        text(name),
                        text(fixed),
                        text(&s)
                    )));
                }
                Some(s)
            }
            None => attr.fixed.clone().or_else(|| attr.def.clone()),
        };
        if let Some(s) = str_val {
            let dummy = new_node(None, b"BOGUS")?;
            if let Some(t) = Node::new_text(None, &s) {
                dummy.add_child(t);
            }
            let v = master_to_zval(req, cx, attr.encode, dummy);
            dummy.free();
            set_zval_property(req, ret, name, v?)?;
        }
    }
    Ok(())
}

/// `to_zval_object_ex` with no WSDL type: each element child a property,
/// repeated ones an array.
fn plain_object(req: &mut Request, cx: &mut Cx, data: Node, ce: ClassId) -> Res<Owner<Value>> {
    let ret = match new_object_ref(req, cx, data, ce)? {
        Fresh::Found(v) => return Ok(v),
        Fresh::Made(v) => v,
    };
    let filled = plain_fill(req, cx, &ret, data);
    kept_or_given_up(req, ret, filled)
}

fn plain_fill(req: &mut Request, cx: &mut Cx, ret: &Value, data: Node) -> Res<()> {
    for trav in crate::xml::children(data) {
        if trav.kind() != Kind::Element {
            continue;
        }
        let tmp = master_to_zval(req, cx, None, trav)?;
        let name = trav.name();
        match get_zval_property(req, ret, &name) {
            None => {
                let repeated = trav
                    .next()
                    .and_then(|n| get_node(Some(n), &name, None))
                    .is_some();
                if repeated {
                    let arr = array_of(req, tmp)?;
                    set_zval_property(req, ret, &name, arr.into_value())?;
                } else {
                    set_zval_property(req, ret, &name, tmp)?;
                }
            }
            Some(prop) => match prop.as_array() {
                Some(arr) => append_to_prop(req, ret, &name, arr, tmp)?,
                None => {
                    let prop = Owner::take(req, prop);
                    let arr = match array_of(req, prop) {
                        Ok(arr) => arr,
                        Err(err) => {
                            tmp.give_up(req);
                            return Err(err);
                        }
                    };
                    let mut view = *arr;
                    if let Err(err) = set_zval_property(req, ret, &name, arr.into_value()) {
                        tmp.give_up(req);
                        return Err(err);
                    }
                    view.append(req, tmp)?;
                }
            },
        }
    }
    Ok(())
}

// ---- writing

/// `model_to_xml_object`'s element written with its name and namespace.
fn name_property(req: &mut Request, el: &crate::model::Type, property: Option<Node>, style: Use) {
    let Some(property) = property else {
        return;
    };
    property.set_name(&el.name.clone().unwrap_or_default());
    if style == Use::Literal
        && let Some(namens) = &el.namens
        && el.form == Form::Qualified
    {
        let nsp = encode_add_ns(req, property, namens);
        property.set_ns_call(nsp);
    }
}

/// One value of an element: nil where it may be, else written and held
/// to the element's `fixed`.
fn element_to_xml(
    req: &mut Request,
    cx: &mut Cx,
    el: &crate::model::Type,
    val: Value,
    style: Use,
    node: Node,
) -> Res<Option<Node>> {
    if matches!(val.deref().as_ref(), ValueRef::Null) && el.nillable {
        let property = new_node(node.doc(), b"BOGUS")?;
        node.add_child(property);
        set_xsi_nil(req, property);
        return Ok(Some(property));
    }
    let property = master_to_xml(req, cx, el.encode, val, style, node)?;
    if let Some(content) = property.and_then(child_content)
        && let Some(fixed) = &el.fixed
        && *fixed != content
    {
        return Err(soap_error(format_args!(
            "Encoding: Element '{}' has fixed value '{}' (value '{}' is not allowed)",
            text(&el.name.clone().unwrap_or_default()),
            text(fixed),
            text(&content)
        )));
    }
    Ok(property)
}

/// `model_to_xml_object`: 1 written, 2 optional and absent, 0 missing.
#[allow(clippy::too_many_lines)]
fn model_to_xml_object(
    req: &mut Request,
    cx: &mut Cx,
    node: Node,
    model: &ContentModel,
    object: &Value,
    style: Use,
    strict: bool,
    sdl: &Arc<Sdl>,
) -> Res<i32> {
    match (&model.u, model.kind) {
        (Content::Element(id), ContentKind::Element) => {
            let Some(el) = sdl.ty(*id) else {
                return Ok(1);
            };
            let name = el.name.clone().unwrap_or_default();
            let data = get_zval_property(req, object, &name);
            if let Some(d) = data
                && matches!(d.as_ref(), ValueRef::Null)
                && !el.nillable
                && model.min_occurs > 0
                && !strict
            {
                return Ok(0);
            }
            if let Some(data) = data {
                if (model.max_occurs == -1 || model.max_occurs > 1)
                    && let Some(arr) = data.as_array()
                    && arr.is_list()
                {
                    let read = rows(req, arr);
                    with_rows(req, &read, |req, rows| {
                        for (_, val) in rows {
                            let property = element_to_xml(req, cx, el, *val, style, node)?;
                            name_property(req, el, property, style);
                        }
                        Ok(())
                    })?;
                } else {
                    if matches!(data.as_ref(), ValueRef::Null)
                        && !el.nillable
                        && model.min_occurs == 0
                    {
                        return Ok(1);
                    }
                    let property = element_to_xml(req, cx, el, data, style, node)?;
                    name_property(req, el, property, style);
                }
                return Ok(1);
            }
            if strict && el.nillable && model.min_occurs > 0 {
                let property = new_node(node.doc(), &name)?;
                node.add_child(property);
                set_xsi_nil(req, property);
                if style == Use::Literal
                    && let Some(namens) = &el.namens
                    && el.form == Form::Qualified
                {
                    let nsp = encode_add_ns(req, property, namens);
                    property.set_ns_call(nsp);
                }
                return Ok(1);
            }
            if model.min_occurs == 0 {
                return Ok(2);
            }
            if strict {
                return Err(soap_error(format_args!(
                    "Encoding: object has no '{}' property",
                    text(&name)
                )));
            }
            Ok(0)
        }
        (_, ContentKind::Any) => {
            let Some(data) = get_zval_property(req, object, b"any") else {
                if model.min_occurs == 0 {
                    return Ok(2);
                }
                if strict {
                    return Err(soap_error("Encoding: object has no 'any' property"));
                }
                return Ok(0);
            };
            let anyxml = get_conversion(ty::XSD_ANYXML)?;
            if (model.max_occurs == -1 || model.max_occurs > 1)
                && let Some(arr) = data.as_array()
                && arr.is_list()
            {
                let read = rows(req, arr);
                with_rows(req, &read, |req, rows| {
                    for (_, val) in rows {
                        master_to_xml(req, cx, Some(anyxml), *val, style, node)?;
                    }
                    Ok(())
                })?;
            } else {
                master_to_xml(req, cx, Some(anyxml), data, style, node)?;
            }
            Ok(1)
        }
        (Content::Particles(list), ContentKind::Sequence | ContentKind::All) => {
            let mut strict = strict;
            for tmp in list {
                let r = model_to_xml_object(
                    req,
                    cx,
                    node,
                    tmp,
                    object,
                    style,
                    strict && tmp.min_occurs > 0,
                    sdl,
                )?;
                if r == 0 && (!strict || tmp.min_occurs > 0) {
                    return Ok(0);
                }
                strict = true;
            }
            Ok(1)
        }
        (Content::Particles(list), ContentKind::Choice) => {
            let mut ret = 0;
            for tmp in list {
                let r = model_to_xml_object(req, cx, node, tmp, object, style, false, sdl)?;
                if r == 1 {
                    return Ok(1);
                }
                if r != 0 {
                    ret = 1;
                }
            }
            Ok(ret)
        }
        (Content::Group(id), ContentKind::Group) => {
            match sdl.ty(*id).and_then(|g| g.model.as_deref()) {
                Some(m) => model_to_xml_object(
                    req,
                    cx,
                    node,
                    m,
                    object,
                    style,
                    strict && model.min_occurs > 0,
                    sdl,
                ),
                None => Ok(1),
            }
        }
        _ => Ok(1),
    }
}

/// `model_array_element`: the one repeated element a model is a list of.
fn model_array_element(model: &ContentModel, sdl: &Sdl) -> Option<TypeId> {
    match (&model.u, model.kind) {
        (Content::Element(id), ContentKind::Element) => {
            (model.max_occurs == -1 || model.max_occurs > 1).then_some(*id)
        }
        (
            Content::Particles(list),
            ContentKind::Sequence | ContentKind::All | ContentKind::Choice,
        ) => match list.as_slice() {
            [one] => model_array_element(one, sdl),
            _ => None,
        },
        (Content::Group(id), _) => model_array_element(sdl.ty(*id)?.model.as_deref()?, sdl),
        _ => None,
    }
}

/// The rows of an object's property table or of an array, the property
/// names unmangled (`zend_unmangle_property_name`).
fn prop_rows(req: &mut Request, data: &Value) -> Res<Vec<(K, Value)>> {
    match data.as_ref() {
        ValueRef::Array(a) => Ok(rows(req, a)),
        ValueRef::Object(o) => {
            let table = o.property_table(req)?;
            Ok(walk::rows(req, table)
                .map(|(k, v)| {
                    let k = match k {
                        Key::Int(n) => K::Int(n),
                        Key::Str(s) => {
                            let b = s.as_bytes();
                            let name = match b.first() {
                                Some(0) => b[1..]
                                    .iter()
                                    .position(|&c| c == 0)
                                    .map_or(b, |at| &b[at + 2..]),
                                _ => b,
                            };
                            K::Str(name.to_vec())
                        }
                    };
                    (k, v)
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// `to_xml_object`.
#[allow(clippy::too_many_lines)]
pub(super) fn to_xml_object(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
) -> Res<Node> {
    if super::is_null(&data) {
        let x = new_node(parent.doc(), b"BOGUS")?;
        parent.add_child(x);
        if style == Use::Encoded {
            set_xsi_nil(req, x);
            set_ns_and_type(req, x, &e.e);
        }
        return Ok(x);
    }
    let has_prop = matches!(data.as_ref(), ValueRef::Object(_) | ValueRef::Array(_));
    let sdl = super::sdl(req);
    let Some(sdl_arc) = sdl.clone() else {
        return plain_to_xml(req, cx, e, data, style, parent, has_prop);
    };
    let Some(t) = sdl_type(Some(&sdl_arc), &e.e) else {
        return plain_to_xml(req, cx, e, data, style, parent, has_prop);
    };
    let fresh = |parent: Node| -> Res<Node> {
        let x = new_node(parent.doc(), b"BOGUS")?;
        parent.add_child(x);
        Ok(x)
    };
    let x_param = match (t.kind, t.encode) {
        (TypeKind::Restriction, Some(te)) if te != e.r => {
            match simple_base(req, sdl.as_ref(), Some(te))? {
                Some(base) => base_value(req, cx, base, data, style, parent, has_prop)?,
                None => fresh(parent)?,
            }
        }
        (TypeKind::Extension, Some(te)) if te != e.r => {
            let base = enc(req, te)?;
            let complex = sdl_type(Some(&sdl_arc), &base.e).is_some_and(|bt| {
                !matches!(bt.kind, TypeKind::Simple | TypeKind::List | TypeKind::Union)
            });
            if complex {
                protect(&data, true);
                let r = master_to_xml(req, cx, Some(te), data, style, parent);
                protect(&data, false);
                match r? {
                    Some(n) => n,
                    None => fresh(parent)?,
                }
            } else {
                base_value(req, cx, te, data, style, parent, has_prop)?
            }
        }
        _ => fresh(parent)?,
    };
    if soap_check_zval_ref(req, cx, &data, x_param) {
        return Ok(x_param);
    }
    if has_prop {
        let array_el = match (data.as_array(), t.attributes.is_none(), t.model.as_deref()) {
            (Some(a), true, Some(m)) if a.is_list() => model_array_element(m, &sdl_arc),
            _ => None,
        };
        if let Some(el_id) = array_el
            && let Some(el) = sdl_arc.ty(el_id)
            && let Some(arr) = data.as_array()
        {
            let read = rows(req, arr);
            with_rows(req, &read, |req, rows| {
                for (_, val) in rows {
                    let property = if matches!(val.deref().as_ref(), ValueRef::Null) && el.nillable
                    {
                        let p = new_node(parent.doc(), b"BOGUS")?;
                        x_param.add_child(p);
                        set_xsi_nil(req, p);
                        Some(p)
                    } else {
                        master_to_xml(req, cx, el.encode, *val, style, x_param)?
                    };
                    name_property(req, el, property, style);
                }
                Ok(())
            })?;
        } else if let Some(m) = t.model.as_deref() {
            model_to_xml_object(req, cx, x_param, m, &data, style, true, &sdl_arc)?;
        }
        if let Some(attrs) = &t.attributes {
            let was = cx.tracking;
            cx.tracking = false;
            let r = attributes_to_xml(req, cx, attrs, &data, x_param);
            cx.tracking = was;
            r?;
        }
    }
    if style == Use::Encoded {
        set_ns_and_type(req, x_param, &e.e);
    }
    Ok(x_param)
}

/// `GC_TRY_PROTECT_RECURSION` and its undoing on an object's or an array's table.
fn protect(data: &Value, on: bool) {
    match (data.as_ref(), on) {
        (ValueRef::Object(o), true) => o.protect(),
        (ValueRef::Object(o), false) => o.unprotect(),
        (ValueRef::Array(a), true) => a.protect(),
        (ValueRef::Array(a), false) => a.unprotect(),
        _ => {}
    }
}

/// A restriction's or a simple extension's value: its `_` property, the
/// value itself where it has no table, else an empty element.
fn base_value(
    req: &mut Request,
    cx: &mut Cx,
    base: EncRef,
    data: Value,
    style: Use,
    parent: Node,
    has_prop: bool,
) -> Res<Node> {
    let written = if let Some(tmp) = get_zval_property(req, &data, b"_") {
        master_to_xml(req, cx, Some(base), tmp, style, parent)?
    } else if !has_prop {
        master_to_xml(req, cx, Some(base), data, style, parent)?
    } else {
        None
    };
    match written {
        Some(n) => Ok(n),
        None => {
            let x = new_node(parent.doc(), b"BOGUS")?;
            parent.add_child(x);
            Ok(x)
        }
    }
}

/// `to_xml_object`'s attributes, each written as a string and set on the node.
fn attributes_to_xml(
    req: &mut Request,
    cx: &mut Cx,
    attrs: &crate::table::Table<crate::model::Attribute>,
    data: &Value,
    x_param: Node,
) -> Res<()> {
    for attr in attrs.values() {
        let Some(name) = &attr.name else {
            continue;
        };
        let Some(zattr) = get_zval_property(req, data, name) else {
            continue;
        };
        let Some(dummy) = master_to_xml(req, cx, attr.encode, zattr, Use::Literal, x_param)? else {
            continue;
        };
        if let Some(content) = child_content(dummy) {
            if let Some(fixed) = &attr.fixed
                && *fixed != content
            {
                return Err(soap_error(format_args!(
                    "Encoding: Attribute '{}' has fixed value '{}' (value '{}' is not allowed)",
                    text(name),
                    text(fixed),
                    text(&content)
                )));
            }
            match &attr.namens {
                Some(namens)
                    if namens.as_slice() == XML_NAMESPACE || attr.form == Form::Qualified =>
                {
                    let nsp = encode_add_ns(req, x_param, namens);
                    x_param.set_ns_prop(nsp, name, &content);
                }
                _ => {
                    x_param.set_prop(name, &content);
                }
            }
        }
        dummy.unlink();
        dummy.free();
    }
    Ok(())
}

/// `to_xml_object` with no WSDL type: each property or element a child
/// named by its key.
fn plain_to_xml(
    req: &mut Request,
    cx: &mut Cx,
    e: &Enc,
    data: Value,
    style: Use,
    parent: Node,
    has_prop: bool,
) -> Res<Node> {
    let x_param = new_node(parent.doc(), b"BOGUS")?;
    parent.add_child(x_param);
    if soap_check_zval_ref(req, cx, &data, x_param) {
        return Ok(x_param);
    }
    if has_prop {
        let rows = prop_rows(req, &data)?;
        with_rows(req, &rows, |req, rows| {
            for (k, zprop) in rows {
                let code = type_code(&zprop.deref());
                let conv = get_conversion(code)?;
                let property = master_to_xml(req, cx, Some(conv), *zprop, style, x_param)?;
                if let (K::Str(name), Some(p)) = (k, property) {
                    p.set_name(name);
                }
            }
            Ok(())
        })?;
    }
    if style == Use::Encoded {
        set_ns_and_type(req, x_param, &e.e);
    }
    Ok(x_param)
}

//! `php_sdl.c`'s caches: `get_sdl`, which answers a description from
//! the request's memory cache (`WSDL_CACHE_MEMORY`), from the cache file
//! (`WSDL_CACHE_DISK`) or by reading the WSDL; and the cache file itself,
//! `<soap.wsdl_cache_dir>/wsdl-<user>-<version><md5>`, written by
//! `add_sdl_to_cache` and read by `get_sdl_from_cache` byte for byte in
//! php's layout, little-endian, so a file php wrote is read here and the
//! other way round.
//!
//! A read answers `Option<Option<_>>` where php reads a pointer that may
//! be `NULL`: the outer `None` is a file cut short, the inner php's
//! marker for none.
#![allow(clippy::option_option)]

use std::collections::HashMap;
use std::sync::Arc;

use php_ext::coerce::atoi;
use php_ext::request::{Ini, MAXPATHLEN, Request, StreamCtx};
use php_ext::values::{Owner, PhpObject, Value, ValueRef};
use php_ext_standard::md5::Md5;
use php_ext_standard::php_get_current_user;

use crate::classes::string;
use crate::http::{basic_authentication, http_context_headers, proxy_authentication};
use crate::ini;
use crate::model::{
    AttrUse, Attribute, Binding, BindingType, Body, Content, ContentKind, ContentModel,
    DEFAULT_ENCODING, EncId, EncRef, EncodeType, Encoder, ExtraAttribute, Fault, Form, Function,
    Header, Param, RestrictionChar, RestrictionInt, Restrictions, RpcEncoding,
    SOAP_1_1_ENC_NAMESPACE, SOAP_1_2_ENC_NAMESPACE, Sdl, SoapBinding, SoapBindingFunction,
    SoapFaultBinding, Style, ToXml, ToZval, Type, TypeId, TypeKind, Use, XSD_NAMESPACE, builtin,
};
use crate::sdl::get_encoder_ex;
use crate::sdl::wsdl::load_wsdl;
use crate::shared::{ClientSlot, client_slot, instance_of};
use crate::table::{Key, Table};
use crate::{Error, Res, State};

/// `WSDL_CACHE_DISK`.
pub(crate) const WSDL_CACHE_DISK: i64 = 0x1;
/// `WSDL_CACHE_MEMORY`.
pub(crate) const WSDL_CACHE_MEMORY: i64 = 0x2;
/// `WSDL_CACHE_VERSION`.
const WSDL_CACHE_VERSION: u8 = 0x10;
/// `WSDL_NO_STRING_MARKER`.
const WSDL_NO_STRING_MARKER: u32 = 0x7fff_ffff;

// ---- writing, `add_sdl_to_cache`

/// The file being written: `WSDL_CACHE_PUT_*`.
struct Out(Vec<u8>);

impl Out {
    /// `WSDL_CACHE_PUT_INT`: the low 32 bits, little-endian.
    fn int(&mut self, v: i64) {
        self.0.extend_from_slice(&low32(v).to_le_bytes());
    }

    /// `WSDL_CACHE_PUT_1`.
    fn one(&mut self, v: u8) {
        self.0.push(v);
    }

    fn len(&mut self, n: usize) {
        self.int(i64::try_from(n).unwrap_or(i64::MAX));
    }

    /// `sdl_serialize_string`.
    fn string(&mut self, s: Option<&[u8]>) {
        match s {
            Some(s) => {
                self.len(s.len());
                self.0.extend_from_slice(s);
            }
            None => self.int(i64::from(WSDL_NO_STRING_MARKER)),
        }
    }

    /// `sdl_serialize_key`: a string key, or the marker for an integer
    /// one.
    fn key(&mut self, k: &Key) {
        match k {
            Key::Str(s) => self.string(Some(s)),
            Key::Int(_) => self.int(i64::from(WSDL_NO_STRING_MARKER)),
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "`WSDL_CACHE_PUT_INT` writes the low four bytes of whatever it is given"
)]
fn low32(v: i64) -> u32 {
    v as u32
}

/// The numbers the file names types and encoders by: 1 on, in the order
/// of the groups, the types and the elements, and of the description's
/// encoders then `defaultEncoding[]`; 0 for none.
struct Nums {
    types: HashMap<TypeId, i64>,
    encoders: HashMap<EncId, i64>,
    first_default: i64,
}

impl Nums {
    /// `sdl_serialize_encoder_ref`.
    fn encoder(&self, r: Option<EncRef>) -> i64 {
        match r {
            Some(EncRef::Sdl(id)) => self.encoders.get(&id).copied().unwrap_or(0),
            Some(EncRef::Default(at)) => self.first_default + i64::from(at),
            _ => 0,
        }
    }

    /// `sdl_serialize_type_ref`.
    fn ty(&self, t: Option<TypeId>) -> i64 {
        t.and_then(|t| self.types.get(&t).copied()).unwrap_or(0)
    }
}

/// `sdl_serialize_attribute`.
fn serialize_attribute(attr: &Attribute, nums: &Nums, out: &mut Out) {
    out.string(attr.name.as_deref());
    out.string(attr.namens.as_deref());
    out.string(attr.ref_.as_deref());
    out.string(attr.def.as_deref());
    out.string(attr.fixed.as_deref());
    out.one(attr.form as u8);
    out.one(attr.use_ as u8);
    out.int(nums.encoder(attr.encode));
    let extra = attr.extra_attributes.as_ref();
    out.len(extra.map_or(0, Table::len));
    for (key, x) in extra.into_iter().flat_map(Table::iter) {
        out.key(key);
        out.string(x.ns.as_deref());
        out.string(x.val.as_deref());
    }
}

/// `sdl_serialize_model`.
fn serialize_model(
    model: &ContentModel,
    nums: &Nums,
    elements: Option<&HashMap<TypeId, i64>>,
    out: &mut Out,
) {
    out.one(model.kind as u8);
    out.int(i64::from(model.min_occurs));
    out.int(i64::from(model.max_occurs));
    match &model.u {
        Content::Element(e) => out.int(elements.and_then(|m| m.get(e).copied()).unwrap_or(0)),
        Content::Particles(list) => {
            out.len(list.len());
            for m in list {
                serialize_model(m, nums, elements, out);
            }
        }
        Content::GroupRef(r) => out.string(Some(r)),
        Content::Group(g) => out.int(nums.ty(Some(*g))),
        Content::None => {}
    }
}

/// `sdl_serialize_resriction_int`.
fn serialize_restriction_int(x: Option<&RestrictionInt>, out: &mut Out) {
    match x {
        Some(x) => {
            out.one(1);
            out.int(i64::from(x.value));
            out.one(u8::from(x.fixed));
        }
        None => out.one(0),
    }
}

/// `sdl_serialize_resriction_char`.
fn serialize_restriction_char(x: Option<&RestrictionChar>, out: &mut Out) {
    match x {
        Some(x) => {
            out.one(1);
            out.string(Some(&x.value));
            out.one(u8::from(x.fixed));
        }
        None => out.one(0),
    }
}

/// `sdl_serialize_type`: the type, its restrictions, its own elements
/// in full, its attributes and its model.
fn serialize_type(sdl: &Sdl, id: TypeId, nums: &Nums, out: &mut Out) {
    let Some(t) = sdl.ty(id) else {
        return;
    };
    out.one(t.kind as u8);
    out.string(t.name.as_deref());
    out.string(t.namens.as_deref());
    out.string(t.def.as_deref());
    out.string(t.fixed.as_deref());
    out.string(t.ref_.as_deref());
    out.one(u8::from(t.nillable));
    out.one(t.form as u8);
    out.int(nums.encoder(t.encode));
    match t.restrictions.as_deref() {
        Some(r) => {
            out.one(1);
            for x in [
                &r.min_exclusive,
                &r.min_inclusive,
                &r.max_exclusive,
                &r.max_inclusive,
                &r.total_digits,
                &r.fraction_digits,
                &r.length,
                &r.min_length,
                &r.max_length,
            ] {
                serialize_restriction_int(x.as_ref(), out);
            }
            serialize_restriction_char(r.white_space.as_ref(), out);
            serialize_restriction_char(r.pattern.as_ref(), out);
            let e = r.enumeration.as_ref();
            out.len(e.map_or(0, Table::len));
            for (key, x) in e.into_iter().flat_map(Table::iter) {
                serialize_restriction_char(Some(x), out);
                out.key(key);
            }
        }
        None => out.one(0),
    }
    let mut local: Option<HashMap<TypeId, i64>> = None;
    let elements = t.elements.as_ref();
    let mut i = elements.map_or(0, Table::len);
    out.len(i);
    if i > 0 {
        let map = local.get_or_insert_with(HashMap::new);
        for (key, e) in elements.into_iter().flat_map(Table::iter) {
            out.key(key);
            serialize_type(sdl, *e, nums, out);
            map.entry(*e)
                .or_insert(i64::try_from(i).unwrap_or(i64::MAX));
            i -= 1;
        }
    }
    let attrs = t.attributes.as_ref();
    out.len(attrs.map_or(0, Table::len));
    for (key, a) in attrs.into_iter().flat_map(Table::iter) {
        out.key(key);
        serialize_attribute(a, nums, out);
    }
    match t.model.as_deref() {
        Some(m) => {
            out.one(1);
            serialize_model(m, nums, local.as_ref(), out);
        }
        None => out.one(0),
    }
}

/// `sdl_serialize_parameters`.
fn serialize_parameters(ht: Option<&Table<Param>>, nums: &Nums, out: &mut Out) {
    out.len(ht.map_or(0, Table::len));
    for (key, p) in ht.into_iter().flat_map(Table::iter) {
        out.key(key);
        out.string(p.param_name.as_deref());
        out.int(i64::from(p.order));
        out.int(nums.encoder(p.encode));
        out.int(nums.ty(p.element));
    }
}

/// One header of a body, as `sdl_serialize_soap_body` writes it.
fn serialize_header(key: &Key, h: &Header, nums: &Nums, out: &mut Out) {
    out.key(key);
    out.one(h.use_ as u8);
    if h.use_ == Use::Encoded {
        out.one(h.encoding_style as u8);
    }
    out.string(Some(&h.name));
    out.string(h.ns.as_deref());
    out.int(nums.encoder(h.encode));
    out.int(nums.ty(h.element));
}

/// `sdl_serialize_soap_body`. Where a header has header faults, php
/// writes their count and then the body's headers again in their place,
/// and so does this.
fn serialize_soap_body(body: &Body, nums: &Nums, out: &mut Out) {
    out.one(body.use_ as u8);
    if body.use_ == Use::Encoded {
        out.one(body.encoding_style as u8);
    }
    out.string(body.ns.as_deref());
    let headers = body.headers.as_ref();
    out.len(headers.map_or(0, Table::len));
    for (key, h) in headers.into_iter().flat_map(Table::iter) {
        serialize_header(key, h, nums, out);
        let j = h.headerfaults.as_ref().map_or(0, Table::len);
        out.len(j);
        if j > 0 {
            for (key2, h2) in headers.into_iter().flat_map(Table::iter) {
                serialize_header(key2, h2, nums, out);
            }
        }
    }
}

/// The bytes of the cache file of `sdl`, read from `uri` at `t`.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn sdl_serialize(sdl: &Sdl, uri: &[u8], t: i64) -> Vec<u8> {
    let mut out = Out(Vec::new());
    out.0.extend_from_slice(b"wsdl");
    out.one(WSDL_CACHE_VERSION);
    out.one(0);
    out.0.extend_from_slice(&t.to_le_bytes());
    out.string(Some(uri));
    out.string(sdl.source.as_deref());
    out.string(sdl.target_ns.as_deref());

    let mut nums = Nums {
        types: HashMap::new(),
        encoders: HashMap::new(),
        first_default: 0,
    };
    let mut type_num = 1;
    for table in [&sdl.groups, &sdl.types, &sdl.elements] {
        out.len(table.as_ref().map_or(0, Table::len));
        for id in table.iter().flat_map(Table::values) {
            nums.types.entry(*id).or_insert(type_num);
            type_num += 1;
        }
    }
    out.len(sdl.encoders.as_ref().map_or(0, Table::len));
    let mut encoder_num = 1;
    for id in sdl.encoders.iter().flat_map(Table::values) {
        nums.encoders.entry(*id).or_insert(encoder_num);
        encoder_num += 1;
    }
    nums.first_default = encoder_num;

    for table in [&sdl.groups, &sdl.types, &sdl.elements] {
        for (key, id) in table.iter().flat_map(Table::iter) {
            out.key(key);
            serialize_type(sdl, *id, &nums, &mut out);
        }
    }
    for (key, id) in sdl.encoders.iter().flat_map(Table::iter) {
        out.key(key);
        if let Some(enc) = sdl.encoder(*id) {
            out.int(i64::from(enc.details.type_));
            out.string(enc.details.type_str.as_deref());
            out.string(enc.details.ns.as_deref());
            out.int(nums.ty(enc.details.sdl_type));
        }
    }

    let bindings = sdl.bindings.as_ref();
    out.len(bindings.map_or(0, Table::len));
    for (key, b) in bindings.into_iter().flat_map(Table::iter) {
        out.key(key);
        out.string(Some(&b.name));
        out.string(b.location.as_deref());
        out.one(b.binding_type as u8);
        match (b.binding_type, b.attributes) {
            (BindingType::Soap, Some(a)) => {
                out.one(a.style as u8);
                out.one(low32(i64::from(a.transport)).to_le_bytes()[0]);
            }
            _ => out.one(0),
        }
    }

    out.len(sdl.functions.len());
    for (key, f) in sdl.functions.iter() {
        out.key(key);
        out.string(Some(&f.function_name));
        out.string(f.request_name.as_deref());
        out.string(f.response_name.as_deref());
        let binding = sdl.binding_of(f);
        if let (Some(at), Some(b)) = (f.binding, binding) {
            out.len(at + 1);
            match (b.binding_type, f.binding_attributes.as_ref()) {
                (BindingType::Soap, Some(fb)) => {
                    out.one(fb.style as u8);
                    out.string(fb.soap_action.as_deref());
                    serialize_soap_body(&fb.input, &nums, &mut out);
                    serialize_soap_body(&fb.output, &nums, &mut out);
                }
                _ => out.one(0),
            }
        }
        serialize_parameters(f.request_parameters.as_ref(), &nums, &mut out);
        serialize_parameters(f.response_parameters.as_ref(), &nums, &mut out);
        match f.faults.as_ref() {
            Some(faults) => {
                out.len(faults.len());
                for (key2, fault) in faults.iter() {
                    out.key(key2);
                    out.string(Some(&fault.name));
                    serialize_parameters(fault.details.as_ref(), &nums, &mut out);
                    let soap = binding.is_some_and(|b| b.binding_type == BindingType::Soap);
                    match (soap, fault.binding.as_ref()) {
                        (true, Some(fb)) => {
                            out.one(fb.use_ as u8);
                            if fb.use_ == Use::Encoded {
                                out.one(fb.encoding_style as u8);
                            }
                            out.string(fb.ns.as_deref());
                        }
                        _ => out.one(0),
                    }
                }
            }
            None => out.int(0),
        }
    }

    let requests = sdl.requests.as_ref();
    out.len(requests.map_or(0, Table::len));
    for (key, at) in requests.into_iter().flat_map(Table::iter) {
        out.len(at + 1);
        out.key(key);
    }
    out.0
}

// ---- reading, `get_sdl_from_cache`

/// The file being read: `WSDL_CACHE_GET_*`, each `None` past its end,
/// which this treats as no cache.
struct In<'a> {
    buf: &'a [u8],
    at: usize,
}

impl In<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let s = self.buf.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }

    /// `WSDL_CACHE_GET_INT`.
    fn int(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// `WSDL_CACHE_GET_INT` into a signed `int`.
    fn sint(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.int()?.to_le_bytes()))
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.int()?).ok()
    }

    /// `WSDL_CACHE_GET_1`.
    fn one(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    /// `**in`, the next byte without taking it.
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.at).copied()
    }

    /// `sdl_deserialize_string`: `None` inside for the marker.
    fn string(&mut self) -> Option<Option<Vec<u8>>> {
        let len = self.int()?;
        if len == WSDL_NO_STRING_MARKER {
            return Some(None);
        }
        let n = usize::try_from(len).ok()?;
        Some(Some(self.take(n)?.to_vec()))
    }

    /// `sdl_deserialize_key`: the value added under the key read, or
    /// pushed for the marker.
    fn key_into<V>(&mut self, table: &mut Table<V>, value: V) -> Option<()> {
        match self.string()? {
            Some(k) => {
                table.add(Key::Str(k), value);
            }
            None => {
                table.push(value);
            }
        }
        Some(())
    }
}

fn type_kind(b: u8) -> Option<TypeKind> {
    Some(match b {
        0 => TypeKind::Simple,
        1 => TypeKind::List,
        2 => TypeKind::Union,
        3 => TypeKind::Complex,
        4 => TypeKind::Restriction,
        5 => TypeKind::Extension,
        _ => return None,
    })
}

fn form(b: u8) -> Option<Form> {
    Some(match b {
        0 => Form::Default,
        1 => Form::Qualified,
        2 => Form::Unqualified,
        _ => return None,
    })
}

fn attr_use(b: u8) -> Option<AttrUse> {
    Some(match b {
        0 => AttrUse::Default,
        1 => AttrUse::Optional,
        2 => AttrUse::Prohibited,
        3 => AttrUse::Required,
        _ => return None,
    })
}

fn content_kind(b: u8) -> Option<ContentKind> {
    Some(match b {
        0 => ContentKind::Element,
        1 => ContentKind::Sequence,
        2 => ContentKind::All,
        3 => ContentKind::Choice,
        4 => ContentKind::GroupRef,
        5 => ContentKind::Group,
        6 => ContentKind::Any,
        _ => return None,
    })
}

fn encoding_use(b: u8) -> Option<Use> {
    Some(match b {
        0 => Use::Unset,
        1 => Use::Encoded,
        2 => Use::Literal,
        _ => return None,
    })
}

fn rpc_encoding(b: u8) -> Option<RpcEncoding> {
    Some(match b {
        0 => RpcEncoding::Default,
        1 => RpcEncoding::Soap11,
        2 => RpcEncoding::Soap12,
        _ => return None,
    })
}

fn style(b: u8) -> Option<Style> {
    Some(match b {
        1 => Style::Rpc,
        2 => Style::Document,
        _ => return None,
    })
}

fn binding_type(b: u8) -> Option<BindingType> {
    Some(match b {
        1 => BindingType::Soap,
        2 => BindingType::Http,
        _ => return None,
    })
}

/// What the numbers of a file name: the types 1 on, and the encoders 1
/// on, the description's then `defaultEncoding[]`.
struct Refs {
    types: usize,
    encoders: usize,
}

impl Refs {
    fn ty(&self, n: u32) -> Option<Option<TypeId>> {
        match usize::try_from(n).ok()? {
            0 => Some(None),
            i if i <= self.types => Some(Some(TypeId(u32::try_from(i - 1).ok()?))),
            _ => None,
        }
    }

    fn encoder(&self, n: u32) -> Option<Option<EncRef>> {
        let i = usize::try_from(n).ok()?;
        if i == 0 {
            return Some(None);
        }
        if i <= self.encoders {
            return Some(Some(EncRef::Sdl(EncId(u32::try_from(i - 1).ok()?))));
        }
        let at = i - self.encoders - 1;
        if at >= DEFAULT_ENCODING.len() {
            return None;
        }
        Some(Some(EncRef::Default(u16::try_from(at).ok()?)))
    }
}

/// `sdl_deserialize_attribute`.
fn deserialize_attribute(r: &Refs, input: &mut In<'_>) -> Option<Attribute> {
    let mut attr = Attribute {
        name: input.string()?,
        namens: input.string()?,
        ref_: input.string()?,
        def: input.string()?,
        fixed: input.string()?,
        form: form(input.one()?)?,
        use_: attr_use(input.one()?)?,
        encode: r.encoder(input.int()?)?,
        extra_attributes: None,
    };
    let n = input.count()?;
    if n > 0 {
        let mut extra = Table::new();
        for _ in 0..n {
            let key = input.string()?;
            let x = ExtraAttribute {
                ns: input.string()?,
                val: input.string()?,
            };
            match key {
                Some(k) => {
                    extra.add(Key::Str(k), x);
                }
                None => {
                    extra.push(x);
                }
            }
        }
        attr.extra_attributes = Some(extra);
    }
    Some(attr)
}

fn deserialize_restriction_int(input: &mut In<'_>) -> Option<Option<RestrictionInt>> {
    if input.one()? != 1 {
        return Some(None);
    }
    let value = input.sint()?;
    Some(Some(RestrictionInt {
        value,
        fixed: input.one()? != 0,
    }))
}

fn deserialize_restriction_char(input: &mut In<'_>) -> Option<Option<RestrictionChar>> {
    if input.one()? != 1 {
        return Some(None);
    }
    let value = input.string()?.unwrap_or_default();
    Some(Some(RestrictionChar {
        value,
        fixed: input.one()? != 0,
    }))
}

/// `sdl_deserialize_model`, `elements` the owning type's by the numbers
/// the file gave them.
fn deserialize_model(
    r: &Refs,
    elements: &[Option<TypeId>],
    input: &mut In<'_>,
) -> Option<ContentModel> {
    let kind = content_kind(input.one()?)?;
    let min_occurs = input.sint()?;
    let max_occurs = input.sint()?;
    let u = match kind {
        ContentKind::Element => {
            let i = input.count()?;
            Content::Element((*elements.get(i)?)?)
        }
        ContentKind::Sequence | ContentKind::All | ContentKind::Choice => {
            let n = input.count()?;
            let mut list = Vec::new();
            for _ in 0..n {
                list.push(deserialize_model(r, elements, input)?);
            }
            Content::Particles(list)
        }
        ContentKind::GroupRef => Content::GroupRef(input.string()?.unwrap_or_default()),
        ContentKind::Group => Content::Group(r.ty(input.int()?)??),
        ContentKind::Any => Content::None,
    };
    Some(ContentModel {
        kind,
        min_occurs,
        max_occurs,
        u,
    })
}

/// `sdl_deserialize_type` into the type `id`.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn deserialize_type(sdl: &mut Sdl, id: TypeId, refs: &Refs, input: &mut In<'_>) -> Option<()> {
    let mut ty = Type {
        kind: type_kind(input.one()?)?,
        name: input.string()?,
        namens: input.string()?,
        def: input.string()?,
        fixed: input.string()?,
        ref_: input.string()?,
        nillable: input.one()? != 0,
        form: form(input.one()?)?,
        encode: refs.encoder(input.int()?)?,
        ..Type::default()
    };
    if input.peek()? == 1 {
        input.one()?;
        let mut rs = Restrictions::default();
        for slot in [
            &mut rs.min_exclusive,
            &mut rs.min_inclusive,
            &mut rs.max_exclusive,
            &mut rs.max_inclusive,
            &mut rs.total_digits,
            &mut rs.fraction_digits,
            &mut rs.length,
            &mut rs.min_length,
            &mut rs.max_length,
        ] {
            *slot = deserialize_restriction_int(input)?;
        }
        rs.white_space = deserialize_restriction_char(input)?;
        rs.pattern = deserialize_restriction_char(input)?;
        let n = input.count()?;
        if n > 0 {
            let mut enumeration = Table::new();
            for _ in 0..n {
                let x = deserialize_restriction_char(input)??;
                input.key_into(&mut enumeration, x)?;
            }
            rs.enumeration = Some(enumeration);
        }
        ty.restrictions = Some(Box::new(rs));
    } else {
        input.one()?;
    }

    let n = input.count()?;
    let mut elements: Vec<Option<TypeId>> = Vec::new();
    if n > 0 {
        elements = vec![None; n + 1];
        let mut table = Table::new();
        let mut i = n;
        while i > 0 {
            let key = input.string()?;
            let eid = sdl.new_type(Type::default());
            deserialize_type(sdl, eid, refs, input)?;
            match key {
                Some(k) => {
                    table.add(Key::Str(k), eid);
                }
                None => {
                    table.push(eid);
                }
            }
            elements[i] = Some(eid);
            i -= 1;
        }
        ty.elements = Some(table);
    }
    let n = input.count()?;
    if n > 0 {
        let mut table = Table::new();
        for _ in 0..n {
            let key = input.string()?;
            let attr = deserialize_attribute(refs, input)?;
            match key {
                Some(k) => {
                    table.add(Key::Str(k), attr);
                }
                None => {
                    table.push(attr);
                }
            }
        }
        ty.attributes = Some(table);
    }
    if input.one()? != 0 {
        ty.model = Some(Box::new(deserialize_model(refs, &elements, input)?));
    }
    *sdl.ty_mut(id)? = ty;
    Some(())
}

/// `sdl_deserialize_encoder`: an encoder of the description; one with no
/// type under a SOAP encoding namespace converts as XML Schema's of its
/// name.
fn deserialize_encoder(r: &Refs, input: &mut In<'_>) -> Option<Encoder> {
    let type_ = input.sint()?;
    let type_str = input.string()?;
    let ns = input.string()?;
    let clark_notation = ns.as_ref().map(|n| {
        let mut c = vec![b'{'];
        c.extend_from_slice(n);
        c.push(b'}');
        c.extend_from_slice(type_str.as_deref().unwrap_or_default());
        c
    });
    let sdl_type = r.ty(input.int()?)?;
    let mut enc = Encoder {
        details: EncodeType {
            type_,
            type_str,
            ns,
            clark_notation,
            sdl_type,
        },
        to_zval: ToZval::SdlGuess,
        to_xml: ToXml::SdlGuess,
    };
    if sdl_type.is_none() {
        let ns = enc.details.ns.as_deref().unwrap_or_default();
        if ns == SOAP_1_1_ENC_NAMESPACE || ns == SOAP_1_2_ENC_NAMESPACE {
            let mut cat = XSD_NAMESPACE.to_vec();
            cat.push(b':');
            cat.extend_from_slice(enc.details.type_str.as_deref().unwrap_or_default());
            if let Some(EncRef::Default(at)) = get_encoder_ex(None, &cat)
                && let Some(row) = builtin(at)
            {
                enc.to_zval = row.to_zval;
                enc.to_xml = row.to_xml;
            }
        }
    }
    Some(enc)
}

/// One header of a body, as `sdl_deserialize_soap_body` reads it.
fn deserialize_header(r: &Refs, input: &mut In<'_>) -> Option<(Option<Vec<u8>>, Header)> {
    let key = input.string()?;
    let use_ = encoding_use(input.one()?)?;
    let encoding_style = if use_ == Use::Encoded {
        rpc_encoding(input.one()?)?
    } else {
        RpcEncoding::Default
    };
    let name = input.string()?.unwrap_or_default();
    let ns = input.string()?;
    let encode = r.encoder(input.int()?)?;
    let element = r.ty(input.int()?)?;
    Some((
        key,
        Header {
            name,
            ns,
            use_,
            encoding_style,
            element,
            encode,
            headerfaults: None,
        },
    ))
}

fn put<V>(table: &mut Table<V>, key: Option<Vec<u8>>, value: V) {
    match key {
        Some(k) => {
            table.add(Key::Str(k), value);
        }
        None => {
            table.push(value);
        }
    }
}

/// `sdl_deserialize_soap_body`.
fn deserialize_soap_body(r: &Refs, input: &mut In<'_>) -> Option<Body> {
    let use_ = encoding_use(input.one()?)?;
    let encoding_style = if use_ == Use::Encoded {
        rpc_encoding(input.one()?)?
    } else {
        RpcEncoding::Default
    };
    let mut body = Body {
        ns: input.string()?,
        use_,
        encoding_style,
        headers: None,
    };
    let n = input.count()?;
    if n > 0 {
        let mut headers = Table::new();
        for _ in 0..n {
            let (key, mut h) = deserialize_header(r, input)?;
            let j = input.count()?;
            if j > 0 {
                let mut faults = Table::new();
                for _ in 0..j {
                    let (k2, h2) = deserialize_header(r, input)?;
                    put(&mut faults, k2, h2);
                }
                h.headerfaults = Some(faults);
            }
            put(&mut headers, key, h);
        }
        body.headers = Some(headers);
    }
    Some(body)
}

/// `sdl_deserialize_parameters`.
fn deserialize_parameters(r: &Refs, input: &mut In<'_>) -> Option<Option<Table<Param>>> {
    let n = input.count()?;
    if n == 0 {
        return Some(None);
    }
    let mut ht = Table::new();
    for _ in 0..n {
        let key = input.string()?;
        let param = Param {
            param_name: input.string()?,
            order: input.sint()?,
            encode: r.encoder(input.int()?)?,
            element: r.ty(input.int()?)?,
        };
        put(&mut ht, key, param);
    }
    Some(Some(ht))
}

/// What a cache file held.
enum Cached {
    /// The header is whole and the body not one to read, cut short or
    /// naming a type or an encoder it does not hold: read as none, and
    /// the WSDL read again replaces the file (#2395).
    Unreadable,
    /// The file is stale or not this URI's, and is removed.
    Stale,
    /// The description, and when it was read.
    Read(Box<Sdl>, i64),
}

/// The bytes of a cache file read: the version, the time against `t`
/// and the URI checked first, then the description.
fn sdl_deserialize(buf: &[u8], uri: &[u8], t: i64) -> Cached {
    if buf.len() < 6 || &buf[..4] != b"wsdl" || buf[4] != WSDL_CACHE_VERSION || buf[5] != 0 {
        return Cached::Stale;
    }
    let mut input = In { buf, at: 6 };
    let Some(old_t) = input.take(8).map(|b| {
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        i64::from_le_bytes(a)
    }) else {
        return Cached::Stale;
    };
    if old_t < t {
        return Cached::Stale;
    }
    let same = input
        .count()
        .and_then(|n| input.take(n))
        .is_some_and(|u| u == uri);
    if !same {
        return Cached::Stale;
    }
    match read_body(&mut input) {
        Some(sdl) => Cached::Read(Box::new(sdl), old_t),
        None => Cached::Unreadable,
    }
}

/// The description after a cache file's header.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn read_body(input: &mut In<'_>) -> Option<Sdl> {
    let mut sdl = Sdl {
        source: input.string()?,
        target_ns: input.string()?,
        ..Sdl::default()
    };
    let num_groups = input.count()?;
    let num_types = input.count()?;
    let num_elements = input.count()?;
    let num_encoders = input.count()?;
    let r = Refs {
        types: num_groups + num_types + num_elements,
        encoders: num_encoders,
    };
    for _ in 0..r.types {
        sdl.new_type(Type::default());
    }
    for _ in 0..num_encoders {
        sdl.new_encoder(Encoder {
            details: EncodeType::default(),
            to_zval: ToZval::SdlGuess,
            to_xml: ToXml::SdlGuess,
        });
    }
    let mut i = 0u32;
    for (count, which) in [(num_groups, 0), (num_types, 1), (num_elements, 2)] {
        if count == 0 {
            continue;
        }
        let mut table = Table::new();
        for _ in 0..count {
            let id = TypeId(i);
            let key = input.string()?;
            put(&mut table, key, id);
            deserialize_type(&mut sdl, id, &r, input)?;
            i += 1;
        }
        match which {
            0 => sdl.groups = Some(table),
            1 => sdl.types = Some(table),
            _ => sdl.elements = Some(table),
        }
    }
    if num_encoders > 0 {
        let mut table = Table::new();
        for n in 0..num_encoders {
            let id = EncId(u32::try_from(n).ok()?);
            let key = input.string()?;
            put(&mut table, key, id);
            let enc = deserialize_encoder(&r, input)?;
            *sdl.encoder_mut(id)? = enc;
        }
        sdl.encoders = Some(table);
    }

    let num_bindings = input.count()?;
    let mut kinds = Vec::new();
    if num_bindings > 0 {
        let mut table = Table::new();
        for _ in 0..num_bindings {
            let key = input.string()?;
            let name = input.string()?.unwrap_or_default();
            let location = input.string()?;
            let bt = binding_type(input.one()?)?;
            let attributes = if bt == BindingType::Soap && input.peek()? != 0 {
                Some(SoapBinding {
                    style: style(input.one()?)?,
                    transport: i32::from(input.one()?),
                })
            } else {
                input.one()?;
                None
            };
            kinds.push(bt);
            put(
                &mut table,
                key,
                Binding {
                    name,
                    location,
                    binding_type: bt,
                    attributes,
                },
            );
        }
        sdl.bindings = Some(table);
    }

    let num_func = input.count()?;
    for _ in 0..num_func {
        let key = input.string()?;
        let mut func = Function {
            function_name: input.string()?.unwrap_or_default(),
            request_name: input.string()?,
            response_name: input.string()?,
            ..Function::default()
        };
        let binding_num = input.count()?;
        func.binding = binding_num.checked_sub(1);
        let soap = func
            .binding
            .is_some_and(|b| kinds.get(b) == Some(&BindingType::Soap));
        if func.binding.is_some_and(|b| b >= kinds.len()) {
            return None;
        }
        if soap && input.peek()? != 0 {
            let st = style(input.one()?)?;
            let soap_action = input.string()?;
            let input_body = deserialize_soap_body(&r, input)?;
            let output_body = deserialize_soap_body(&r, input)?;
            func.binding_attributes = Some(SoapBindingFunction {
                soap_action,
                style: st,
                input: input_body,
                output: output_body,
            });
        } else {
            input.one()?;
        }
        func.request_parameters = deserialize_parameters(&r, input)?;
        func.response_parameters = deserialize_parameters(&r, input)?;
        let num_faults = input.count()?;
        if num_faults > 0 {
            let mut faults = Table::new();
            for _ in 0..num_faults {
                let k = input.string()?;
                let mut fault = Fault {
                    name: input.string()?.unwrap_or_default(),
                    details: deserialize_parameters(&r, input)?,
                    binding: None,
                };
                if input.peek()? != 0 {
                    let use_ = encoding_use(input.one()?)?;
                    let encoding_style = if use_ == Use::Encoded {
                        rpc_encoding(input.one()?)?
                    } else {
                        RpcEncoding::Default
                    };
                    fault.binding = Some(SoapFaultBinding {
                        ns: input.string()?,
                        use_,
                        encoding_style,
                    });
                } else {
                    input.one()?;
                }
                put(&mut faults, k, fault);
            }
            func.faults = Some(faults);
        }
        put(&mut sdl.functions, key, func);
    }

    let n = input.count()?;
    if n > 0 {
        let mut requests = Table::new();
        for _ in 0..n {
            let function_num = input.count()?;
            let at = function_num.checked_sub(1).filter(|at| *at < num_func)?;
            let key = input.string()?;
            put(&mut requests, key, at);
        }
        sdl.requests = Some(requests);
    }
    Some(sdl)
}

/// `get_sdl_from_cache`: the description in the file at `path` where it
/// is this version's, no older than `t` and of `uri`; a file that is not
/// removed, so the WSDL is read again and written in its place. A file
/// whose header is whole and whose body is cut short or names what it
/// does not hold reads as none, and the WSDL read again replaces it, as
/// does a file cut inside its header.
///
/// ```php
/// $d = sys_get_temp_dir() . '/soap-cache-' . getmypid(); @mkdir($d); ini_set('soap.wsdl_cache_dir', $d); $w = "$d/t.wsdl";
/// file_put_contents($w, "<definitions xmlns='http://schemas.xmlsoap.org/wsdl/' xmlns:s='http://schemas.xmlsoap.org/wsdl/soap/' xmlns:t='urn:t' targetNamespace='urn:t'><portType name='P'/><binding name='B' type='t:P'><s:binding transport='http://schemas.xmlsoap.org/soap/http'/></binding><service name='S'><port name='p' binding='t:B'><s:address location='http://localhost/'/></port></service></definitions>");
/// new SoapClient($w, ['cache_wsdl' => WSDL_CACHE_DISK]); [$f] = glob("$d/wsdl-*"); $full = file_get_contents($f);
/// foreach (['version' => 'wsdl' . chr(9) . substr($full, 5), 'time' => substr_replace($full, str_repeat("\0", 8), 6, 8), 'uri' => substr_replace($full, 'X', 18, 1)] as $what => $bad) {
///     file_put_contents($f, $bad); new SoapClient($w, ['cache_wsdl' => WSDL_CACHE_DISK]); clearstatcache(); $now = file_get_contents($f);
///     var_dump($what, strlen($now) === strlen($full) && $now[4] === $full[4] && substr($now, 6, 8) !== str_repeat("\0", 8) && substr($now, 14) === substr($full, 14));
/// }
/// array_map('unlink', glob("$d/*")); rmdir($d);
/// ```
/// ```expect
/// string(7) "version"
/// bool(true)
/// string(4) "time"
/// bool(true)
/// string(3) "uri"
/// bool(true)
/// ```
fn get_sdl_from_cache(path: &std::path::Path, uri: &[u8], t: i64) -> Option<(Sdl, i64)> {
    let buf = std::fs::read(path).ok()?;
    match sdl_deserialize(&buf, uri, t) {
        Cached::Read(sdl, cached) => Some((*sdl, cached)),
        Cached::Stale => {
            std::fs::remove_file(path).ok();
            None
        }
        Cached::Unreadable => None,
    }
}

/// `add_sdl_to_cache`: the file written beside it under a temporary
/// name and renamed into place, so no reader sees half of it; nothing
/// where the directory cannot be written.
fn add_sdl_to_cache(dir: &std::path::Path, path: &std::path::Path, uri: &[u8], t: i64, sdl: &Sdl) {
    use std::io::Write;
    let Some(mut temp) = (0..16).find_map(|n| {
        let candidate = dir.join(temp_name(n));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .ok()
            .map(|f| (f, candidate))
    }) else {
        return;
    };
    let bytes = sdl_serialize(sdl, uri, t);
    let valid = temp.0.write_all(&bytes).is_ok();
    drop(temp.0);
    if valid && std::fs::rename(&temp.1, path).is_ok() {
        return;
    }
    std::fs::remove_file(&temp.1).ok();
}

/// `tmp.wsdl.XXXXXX`, as `mkstemp` names one.
fn temp_name(attempt: u32) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut seed = u64::from(nanos) ^ (u64::from(std::process::id()) << 32) ^ u64::from(attempt);
    let mut name = String::from("tmp.wsdl.");
    for _ in 0..6 {
        let at = usize::try_from(seed % 62).unwrap_or(0);
        name.push(char::from(ALPHABET[at]));
        seed = seed / 62 + 0x9e37_79b9;
    }
    name
}

// ---- `get_sdl`

/// `cache_wsdl` as `SoapClient` and `SoapServer` default it:
/// `soap.wsdl_cache` (`OnUpdateCacheMode`'s `char`) where
/// `soap.wsdl_cache_enabled` is on, else none.
pub(crate) fn default_cache_wsdl(req: &Request) -> i64 {
    let enabled = req
        .ini()
        .get(ini::CACHE_ENABLED)
        .is_some_and(Ini::parse_bool);
    if !enabled {
        return 0;
    }
    let mode = req.ini().get(ini::CACHE).map_or(0, atoi);
    i64::from(i8::from_le_bytes([mode.to_le_bytes()[0]]))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The cache file's name: `<dir>/wsdl-<user>-<version><md5 of the URI
/// and the long size and byte order>`.
fn cache_key(req: &mut Request, dir: &[u8], uri: &[u8]) -> Vec<u8> {
    let mut md5 = Md5::new();
    md5.update(uri);
    md5.update(&[8, 0]);
    let digest = md5.finish();
    let user = php_get_current_user(req);
    let mut key = dir.to_vec();
    key.extend_from_slice(b"/wsdl-");
    key.extend_from_slice(&user);
    key.push(b'-');
    let hex = |n: u8| if n <= 9 { b'0' + n } else { b'a' + n - 10 };
    key.push(hex(0));
    key.push(hex(WSDL_CACHE_VERSION & 0xf));
    for b in digest {
        key.push(hex(b >> 4));
        key.push(hex(b & 0xf));
    }
    key
}

/// `get_sdl`: the description at `uri` for the client or server `this`,
/// through php's caches.
pub(crate) fn get_sdl(
    req: &mut Request,
    this: PhpObject,
    uri: &[u8],
    cache_wsdl: i64,
) -> Res<Arc<Sdl>> {
    let mut cache_wsdl = cache_wsdl;
    let mut t = now();
    let mut uri = uri.to_vec();
    let mut uri_len = 0;
    if uri.contains(&b':') || uri.first() == Some(&b'/') {
        uri_len = uri.len();
    } else {
        match req.environment_mut().realpath(&uri) {
            None => cache_wsdl = 0,
            Some(path) => {
                uri = path.as_os_str().as_encoded_bytes().to_vec();
                uri_len = uri.len();
            }
        }
    }
    let key_uri = uri[..uri_len].to_vec();
    let ttl = req.ini().long(ini::CACHE_TTL);

    if cache_wsdl & WSDL_CACHE_MEMORY != 0
        && let Some((sdl, time)) = req.state::<State>().mem_cache.get_str(&key_uri)
        && *time >= t.saturating_sub(ttl)
    {
        // An expired entry stays: php deletes it from `EG(persistent_list)`.
        return Ok(Arc::clone(sdl));
    }

    let mut key = None;
    if cache_wsdl & WSDL_CACHE_DISK != 0 && uri_len < MAXPATHLEN {
        let dir = req.ini().get(ini::CACHE_DIR).unwrap_or_default().to_vec();
        let name = cache_key(req, &dir, &key_uri);
        let path = req.environment().absolutize(&name);
        if let Some((sdl, cached)) = get_sdl_from_cache(&path, &key_uri, now().saturating_sub(ttl))
        {
            t = cached;
            return Ok(cache_in_memory(req, cache_wsdl, &key_uri, t, sdl));
        }
        key = Some((req.environment().absolutize(&dir), path));
    }

    let sdl = fetch(req, this, &uri, uri_len)?;
    if cache_wsdl & WSDL_CACHE_DISK != 0
        && let Some((dir, path)) = key
    {
        add_sdl_to_cache(&dir, &path, &key_uri, t, &sdl);
    }
    Ok(cache_in_memory(req, cache_wsdl, &key_uri, t, sdl))
}

/// A client's `stream_context`, one owner more, its proxy set on it (on
/// one made for it where it has none), and its user agent and
/// credentials added to `headers`: whether `Authorization` and
/// `Proxy-Authorization` were.
fn client_context(
    req: &mut Request,
    this: PhpObject,
    uri: &[u8],
    uri_len: usize,
    headers: &mut Vec<u8>,
    context: &mut Option<Owner<StreamCtx>>,
) -> Res<(bool, bool)> {
    let tmp = client_slot(this, ClientSlot::StreamCtx);
    if let Some(r) = tmp.as_resource().copied() {
        // `php_stream_context_from_zval(tmp, 0)`, one owner more.
        let c = StreamCtx::fetch(req, r)?;
        *context = Some(Owner::take(req, c));
    }
    let agent = client_slot(this, ClientSlot::UserAgent);
    if let Some(a) = agent.as_str_bytes().filter(|a| !a.is_empty()) {
        headers.extend_from_slice(b"User-Agent: ");
        headers.extend_from_slice(a);
        headers.extend_from_slice(b"\r\n");
    }
    let mut has_proxy_authorization = false;
    let proxy_host = client_slot(this, ClientSlot::ProxyHost);
    let proxy_port = client_slot(this, ClientSlot::ProxyPort);
    if let (Some(host), ValueRef::Int(port)) = (proxy_host.as_str_bytes(), proxy_port.as_ref()) {
        let mut proxy = b"tcp://".to_vec();
        proxy.extend_from_slice(host);
        proxy.push(b':');
        proxy.extend_from_slice(port.to_string().as_bytes());
        let ctx = if let Some(c) = context {
            **c
        } else {
            let c = StreamCtx::alloc(req)?;
            let view = *c;
            *context = Some(c);
            view
        };
        let value = string(req, &proxy)?;
        let set = ctx.set_option(req, b"http", b"proxy", &value);
        value.give_up(req);
        set?;
        let https = uri_len >= 8 && uri[..8].eq_ignore_ascii_case(b"https://");
        if !https {
            ctx.set_option(req, b"http", b"request_fulluri", &Value::bool(true))?;
        }
        has_proxy_authorization = proxy_authentication(this, headers);
    }
    let has_authorization = basic_authentication(this, headers);
    Ok((has_authorization, has_proxy_authorization))
}

/// The client's `stream_context`, user agent, proxy and credentials put
/// on the stream libxml loads with, and the WSDL read under the `WSDL`
/// code; both put back after.
fn fetch(req: &mut Request, this: PhpObject, uri: &[u8], uri_len: usize) -> Res<Sdl> {
    let old_error_code = req.state::<State>().error_code;
    let mut headers = Vec::new();
    // The context libxml loads with, one owner, the switch's below.
    let mut context: Option<Owner<StreamCtx>> = None;
    let mut has_proxy_authorization = false;
    let mut has_authorization = false;
    let client = if instance_of(req, this, "SoapClient") {
        client_context(req, this, uri, uri_len, &mut headers, &mut context).map(|(a, p)| {
            has_authorization = a;
            has_proxy_authorization = p;
        })
    } else {
        Ok(())
    };
    let made = match client {
        Ok(()) => match context {
            Some(c) => Ok(c),
            None => StreamCtx::alloc(req).map_err(Error::from),
        },
        Err(e) => {
            if let Some(c) = context {
                c.give_up(req);
            }
            Err(e)
        }
    };
    let ctx = made?;
    let options = (|| -> Res<()> {
        if ctx.option(req, b"http", b"protocol_version").is_none() {
            ctx.set_option(req, b"http", b"protocol_version", &Value::float(1.1))?;
            headers.extend_from_slice(b"Connection: close\r\n");
        }
        if !headers.is_empty() {
            http_context_headers(
                req,
                Some(*ctx),
                has_authorization,
                has_proxy_authorization,
                false,
                &mut headers,
            );
            let value = string(req, &headers)?;
            let set = ctx.set_option(req, b"http", b"header", &value);
            value.give_up(req);
            set?;
        }
        Ok(())
    })();
    if let Err(e) = options {
        ctx.give_up(req);
        return Err(e);
    }
    let given = ctx.into_resource().into_value();
    let orig_context = php_ext_libxml::switch_stream_ctx(req, Some(given));
    req.state::<State>().error_code = Some("WSDL");
    // WSDL code: an error leaves here as php's bailout leaves
    // `load_wsdl`, with `SOAP_GLOBAL(error_code)` still `WSDL` and the
    // client's stream still libxml's, for the fault it becomes. What a
    // throwing client raises meanwhile below the fatal levels, soap's
    // handler drops ([`crate::server::soap_error_handler`]).
    let loaded = load_wsdl(req, uri);
    let sdl = match loaded {
        Ok(sdl) => sdl,
        Err(e) => {
            // php's bailout leaves `orig_context` where it was copied,
            // unrestored; the copy's owner is given up here.
            if let Some(orig) = orig_context {
                orig.give_up(req);
            }
            return Err(e);
        }
    };
    req.state::<State>().error_code = old_error_code;
    if let Some(new_context) = php_ext_libxml::switch_stream_ctx(req, orig_context) {
        new_context.give_up(req);
    }
    Ok(sdl)
}

/// `cache_in_memory`: the description kept in the request's memory
/// cache under `WSDL_CACHE_MEMORY`, the oldest entry evicted at
/// `soap.wsdl_cache_limit`, or none kept where none is older than this
/// one.
fn cache_in_memory(req: &mut Request, cache_wsdl: i64, uri: &[u8], t: i64, sdl: Sdl) -> Arc<Sdl> {
    let sdl = Arc::new(sdl);
    if cache_wsdl & WSDL_CACHE_MEMORY == 0 {
        return sdl;
    }
    let limit = req.ini().long(ini::CACHE_LIMIT);
    let cache = &mut req.state::<State>().mem_cache;
    let full = i64::try_from(cache.len()).unwrap_or(i64::MAX);
    if limit > 0 && limit <= full {
        let mut latest = t;
        let mut latest_key = None;
        for (k, (_, time)) in cache.iter() {
            if *time < latest {
                latest = *time;
                latest_key = Some(k.clone());
            }
        }
        match latest_key {
            Some(k) => {
                cache.remove(&k);
            }
            None => return sdl,
        }
    }
    cache.update(Key::Str(uri.to_vec()), (Arc::clone(&sdl), t));
    sdl
}

#[cfg(test)]
mod tests {
    use super::{Cached, In, sdl_deserialize, sdl_serialize};

    /// The file, kept as `xxd -p` wrote it out.
    fn golden() -> Vec<u8> {
        let hex: Vec<u8> = include_str!("cache.golden.hex")
            .bytes()
            .filter(u8::is_ascii_hexdigit)
            .collect();
        hex.chunks(2)
            .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
            .collect()
    }

    /// A cache file php 8.5 wrote for a WSDL with a complex type, an
    /// enumeration, a repeating choice, a SOAP-ENC part, a header, a
    /// fault, a `parts` body, a one-way operation and two ports over one
    /// binding: read here and written back, it is the same bytes.
    #[test]
    fn php_cache_file_round_trips() {
        let golden = &golden();
        let mut input = In {
            buf: golden,
            at: 14,
        };
        let uri = input
            .count()
            .and_then(|n| input.take(n))
            .map(<[u8]>::to_vec)
            .unwrap_or_default();
        let mut t = [0u8; 8];
        t.copy_from_slice(&golden[6..14]);
        let t = i64::from_le_bytes(t);
        let read = sdl_deserialize(golden, &uri, t);
        assert!(matches!(read, Cached::Read(..)), "php's file did not read");
        if let Cached::Read(sdl, cached) = read {
            assert_eq!(cached, t);
            assert_eq!(sdl.functions.len(), 4);
            assert_eq!(sdl.bindings.as_ref().map(crate::table::Table::len), Some(2));
            assert_eq!(&sdl_serialize(&sdl, &uri, t), golden);
        }
    }

    #[test]
    fn a_stale_file_is_refused() {
        let golden = &golden();
        assert!(matches!(
            sdl_deserialize(golden, b"/elsewhere", 0),
            Cached::Stale
        ));
        assert!(matches!(
            sdl_deserialize(golden, b"", i64::MAX),
            Cached::Stale
        ));
    }
}

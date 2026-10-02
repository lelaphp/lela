//! `php_sdl.c`: the encoder and element lookups every layer resolves a
//! type name with.

use php_ext_libxml::ffi::Node;

use crate::Typemap;
use crate::model::{
    EncNs, EncRef, Encoder, SOAP_1_1_ENC_NAMESPACE, SOAP_1_2_ENC_NAMESPACE, Sdl, TypeId,
    XSD_NAMESPACE, builtin, default_by_name,
};
use crate::table::Key;
use crate::xml::{href, parse_namespace};

/// `ns:type`, the key the encoders and elements are found by.
pub(crate) fn nscat(ns: Option<&[u8]>, type_: &[u8]) -> Vec<u8> {
    let ns = ns.unwrap_or_default();
    let mut out = Vec::with_capacity(ns.len() + 1 + type_.len());
    out.extend_from_slice(ns);
    out.push(b':');
    out.extend_from_slice(type_);
    out
}

/// `get_encoder_ex`: the default encoder named `nscat`, else the
/// description's.
pub(crate) fn get_encoder_ex(sdl: Option<&Sdl>, nscat: &[u8]) -> Option<EncRef> {
    if let Some(at) = default_by_name(nscat) {
        return Some(EncRef::Default(at));
    }
    let id = *sdl?.encoders.as_ref()?.get(&Key::str(nscat))?;
    Some(EncRef::Sdl(id))
}

/// `get_encoder`: `ns:type`, falling back from a SOAP encoding namespace
/// to XML Schema's, the copy php adds to the description then named by
/// the namespace it was asked for ([`EncRef::DefaultIn`]).
pub(crate) fn get_encoder(sdl: Option<&Sdl>, ns: Option<&[u8]>, type_: &[u8]) -> Option<EncRef> {
    let key = nscat(ns, type_);
    if let Some(found) = get_encoder_ex(sdl, &key) {
        return Some(found);
    }
    let enc_ns = match ns {
        Some(n) if n == SOAP_1_1_ENC_NAMESPACE => EncNs::Soap11,
        Some(n) if n == SOAP_1_2_ENC_NAMESPACE => EncNs::Soap12,
        _ => return None,
    };
    match get_encoder_ex(None, &nscat(Some(XSD_NAMESPACE), type_))? {
        EncRef::Default(at) if sdl.is_some() => Some(EncRef::DefaultIn(at, enc_ns)),
        found => Some(found),
    }
}

/// `get_encoder_from_prefix`: `prefix:type` resolved through the
/// namespaces in scope at `node`.
pub(crate) fn get_encoder_from_prefix(
    sdl: Option<&Sdl>,
    node: Node,
    type_: &[u8],
) -> Option<EncRef> {
    let (cptype, prefix) = parse_namespace(type_);
    match node.search_ns(prefix) {
        Some(ns) => {
            let h = href(ns);
            get_encoder(sdl, Some(&h), cptype).or_else(|| get_encoder_ex(sdl, cptype))
        }
        None => get_encoder_ex(sdl, type_),
    }
}

/// `get_element`: the element `prefix:name` names, by its namespace in
/// scope at `node`, else by the name as written.
pub(crate) fn get_element(sdl: &Sdl, node: Node, type_: &[u8]) -> Option<TypeId> {
    let elements = sdl.elements.as_ref()?;
    let (cptype, prefix) = parse_namespace(type_);
    match node.search_ns(prefix) {
        Some(ns) => {
            let h = href(ns);
            elements
                .get(&Key::Str(nscat(Some(&h), cptype)))
                .or_else(|| elements.get(&Key::str(type_)))
                .copied()
        }
        None => elements.get(&Key::str(type_)).copied(),
    }
}

/// The `encode` an `encodePtr` names, as the running call reads it: a
/// default row (copied under the namespace `get_encoder` asked for), the
/// description's, or the typemap's.
pub(crate) fn encoder(sdl: Option<&Sdl>, typemap: Option<&Typemap>, r: EncRef) -> Option<Encoder> {
    match r {
        EncRef::Default(at) => Some(builtin(at)?.encoder()),
        EncRef::DefaultIn(at, ns) => {
            let mut enc = builtin(at)?.encoder();
            enc.details.ns = Some(ns.href().to_vec());
            Some(enc)
        }
        EncRef::Sdl(id) => sdl?.encoder(id).cloned(),
        EncRef::Typemap(at) => typemap?.nth(at as usize).map(|t| t.enc.clone()),
    }
}

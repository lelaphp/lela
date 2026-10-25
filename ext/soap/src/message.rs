//! The messages `soap.c` writes: a client's request
//! (`serialize_function_call`), a server's response or fault
//! (`serialize_response_call`), the parameters in them
//! (`serialize_parameter`, `serialize_zval`), the description's lookups
//! they read (`get_param`, `get_function`, `get_doc_function`), the
//! header attributes, the typemap a client or a server is given
//! (`soap_create_typemap`) and the signatures `__getFunctions()` and
//! `__getTypes()` print.

use std::ops::ControlFlow;

use php_ext::call::lookup_class;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::walk::Hold;
use php_ext::values::{
    Key as PhpKey, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, walk,
};
use php_ext_libxml::ffi::{Node, Ns};

use crate::classes::FaultSlot;
use crate::encoding::{Cx, encode_add_ns, encode_finish, encode_reset_ns, master_to_xml};
use crate::model::{
    self, Binding, BindingType, Content, ContentKind, ContentModel, EncRef, Function, Header,
    Param, RPC_SOAP12_NAMESPACE, RPC_SOAP12_NS_PREFIX, SOAP_1_1_ACTOR_NEXT, SOAP_1_1_ENC_NAMESPACE,
    SOAP_1_1_ENC_NS_PREFIX, SOAP_1_1_ENV_NAMESPACE, SOAP_1_1_ENV_NS_PREFIX, SOAP_1_2_ACTOR_NEXT,
    SOAP_1_2_ACTOR_NONE, SOAP_1_2_ACTOR_UNLIMATERECEIVER, SOAP_1_2_ENC_NAMESPACE,
    SOAP_1_2_ENC_NS_PREFIX, SOAP_1_2_ENV_NAMESPACE, SOAP_1_2_ENV_NS_PREFIX, Sdl,
    SoapBindingFunction, Style, Type, TypeKind, Use, WSDL_NAMESPACE, XML_NAMESPACE, XSD_NAMESPACE,
    XSD_NS_PREFIX,
};
use crate::shared::{
    ClientSlot, HEADER_ACTOR, HEADER_DATA, HEADER_MUST_UNDERSTAND, HEADER_NAME, HEADER_NAMESPACE,
    PARAM_DATA, PARAM_NAME, client_slot, fault_slot, instance_of, is_null, slot,
};
use crate::table::{Key, Table};
use crate::throwables::Throwables;
use crate::{Res, SOAP_1_1, SOAP_1_2, Typemap, TypemapEntry, soap_error};

// ---- the description's lookups

/// The binding a function names.
pub(crate) fn binding_of<'a>(sdl: &'a Sdl, function: &Function) -> Option<&'a Binding> {
    sdl.binding_of(function)
}

/// `function->binding->bindingType == BINDING_SOAP`, and the function's
/// SOAP binding where it is.
pub(crate) fn soap_binding<'a>(
    sdl: Option<&Sdl>,
    function: Option<&'a Function>,
) -> Option<&'a SoapBindingFunction> {
    let function = function?;
    let binding = binding_of(sdl?, function)?;
    if binding.binding_type != BindingType::Soap {
        return None;
    }
    function.binding_attributes.as_ref()
}

/// Whether the function's binding is a SOAP one.
pub(crate) fn is_soap_bound(sdl: Option<&Sdl>, function: Option<&Function>) -> bool {
    match (sdl, function) {
        (Some(sdl), Some(f)) => {
            binding_of(sdl, f).is_some_and(|b| b.binding_type == BindingType::Soap)
        }
        _ => false,
    }
}

/// `get_param`: a parameter of the request or the response by name
/// (key first, then `paramName`), or by position.
pub(crate) fn get_param<'a>(
    function: Option<&'a Function>,
    param_name: Option<&[u8]>,
    index: u64,
    response: bool,
) -> Option<&'a Param> {
    let function = function?;
    let ht = if response {
        function.response_parameters.as_ref()
    } else {
        function.request_parameters.as_ref()
    }?;
    match param_name {
        Some(name) => ht
            .get_str(name)
            .or_else(|| ht.values().find(|p| p.param_name.as_deref() == Some(name))),
        None => ht.get_int(index),
    }
}

/// `get_function`: by lowercased name, then by request element name.
pub(crate) fn get_function<'a>(sdl: Option<&'a Sdl>, name: &[u8]) -> Option<&'a Function> {
    let sdl = sdl?;
    let lc = name.to_ascii_lowercase();
    if let Some(f) = sdl.functions.get_str(&lc) {
        return Some(f);
    }
    let at = *sdl.requests.as_ref()?.get_str(&lc)?;
    sdl.functions.nth(at)
}

/// `get_doc_function`: the first document-style function whose request
/// parameters are the elements from `params` on, or with none for none.
pub(crate) fn get_doc_function(sdl: Option<&Sdl>, params: Option<Node>) -> Option<&Function> {
    let sdl = sdl?;
    for f in sdl.functions.values() {
        let Some(fnb) = soap_binding(Some(sdl), Some(f)) else {
            continue;
        };
        if fnb.style != Style::Document {
            continue;
        }
        let count = f.request_parameters.as_ref().map_or(0, Table::len);
        match params {
            None => {
                if count == 0 {
                    return Some(f);
                }
            }
            Some(first) if count > 0 => {
                let mut ok = true;
                let mut node = Some(first);
                for param in f.request_parameters.iter().flat_map(Table::values) {
                    let Some(n) = node else {
                        ok = false;
                        break;
                    };
                    if let Some(element) = param.element.and_then(|e| sdl.ty(e)) {
                        if element.name.as_deref().unwrap_or_default() != n.name().as_slice() {
                            ok = false;
                            break;
                        }
                        match (&element.namens, n.ns()) {
                            (Some(ns), Some(nns)) => {
                                if ns.as_slice() != crate::xml::href(nns).as_slice() {
                                    ok = false;
                                    break;
                                }
                            }
                            (None, None) => {}
                            _ => {
                                ok = false;
                                break;
                            }
                        }
                    } else if param.param_name.as_deref().unwrap_or_default() != n.name().as_slice()
                    {
                        ok = false;
                        break;
                    }
                    node = n.next();
                }
                if ok {
                    return Some(f);
                }
            }
            Some(_) => {}
        }
    }
    None
}

/// The `details.type_str` of an encoder, where it has one.
pub(crate) fn type_str(sdl: Option<&Sdl>, enc: Option<EncRef>) -> Option<Vec<u8>> {
    crate::sdl::encoder(sdl, None, enc?)?.details.type_str
}

// ---- serialising

/// `encode_add_ns` over php's `char *ns`, none for its `NULL`.
fn add_ns(req: &mut Request, node: Node, ns: Option<&[u8]>) -> Option<Ns> {
    ns.and_then(|n| encode_add_ns(req, node, n))
}

/// `xmlBuildQName(ncname, prefix, NULL, 0)`.
fn build_qname(ncname: &[u8], prefix: Option<&[u8]>) -> Vec<u8> {
    match prefix {
        Some(p) if !p.is_empty() => {
            let mut out = p.to_vec();
            out.push(b':');
            out.extend_from_slice(ncname);
            out
        }
        _ => ncname.to_vec(),
    }
}

/// `xmlNodeSetLang`: `xml:lang` set where the XML namespace is found.
fn set_lang(node: Node, lang: &[u8]) {
    if let Some(ns) = node.search_ns_by_href(node.doc(), XML_NAMESPACE) {
        node.set_ns_prop(Some(ns), b"lang", lang);
    }
}

/// `set_soap_header_attributes`: `mustUnderstand` and the actor or role
/// a `SoapHeader` asks for.
pub(crate) fn set_soap_header_attributes(h: Node, header: PhpObject, version: i64) {
    if matches!(
        slot(header, HEADER_MUST_UNDERSTAND).as_ref(),
        ValueRef::Bool(true)
    ) {
        if version == SOAP_1_1 {
            h.set_prop(b"SOAP-ENV:mustUnderstand", b"1");
        } else {
            h.set_prop(b"env:mustUnderstand", b"true");
        }
    }
    let actor = slot(header, HEADER_ACTOR);
    match actor.as_ref() {
        ValueRef::Str(s) => {
            if version == SOAP_1_1 {
                h.set_prop(b"SOAP-ENV:actor", s.as_bytes());
            } else {
                h.set_prop(b"env:role", s.as_bytes());
            }
        }
        ValueRef::Int(n) => {
            if version == SOAP_1_1 {
                if n == 1 {
                    h.set_prop(b"SOAP-ENV:actor", SOAP_1_1_ACTOR_NEXT);
                }
            } else if n == 1 {
                h.set_prop(b"env:role", SOAP_1_2_ACTOR_NEXT);
            } else if n == 2 {
                h.set_prop(b"env:role", SOAP_1_2_ACTOR_NONE);
            } else if n == 3 {
                h.set_prop(b"env:role", SOAP_1_2_ACTOR_UNLIMATERECEIVER);
            }
        }
        _ => {}
    }
}

/// `serialize_parameter`: a `SoapParam` unwrapped into its name and data,
/// the name the description gives, the one given, or `paramN`.
pub(crate) fn serialize_parameter(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&Sdl>,
    param: Option<&Param>,
    param_val: Value,
    index: u32,
    name: Option<&[u8]>,
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let mut val = param_val.deref();
    let mut name = name.map(<[u8]>::to_vec);
    if let ValueRef::Object(o) = val.as_ref() {
        let is_param = lookup_class(req, b"SoapParam")?.is_some_and(|c| o.class() == c);
        if is_param {
            let pname = slot(o, PARAM_NAME);
            let pdata = o.slot(PARAM_DATA).map(|v| v.deref());
            if let (Some(n), Some(d)) = (pname.as_str_bytes(), pdata)
                && !matches!(d.as_ref(), ValueRef::Undef)
            {
                name = Some(n.to_vec());
                val = d;
            }
        }
    }
    let param_name = match param.and_then(|p| p.param_name.clone()) {
        Some(n) => n,
        None => name.unwrap_or_else(|| format!("param{index}").into_bytes()),
    };
    serialize_zval(req, cx, sdl, val, param, &param_name, style, parent)
}

/// `serialize_zval`: the value through its parameter's encoder, a missing
/// one written as the element's `fixed` or its `default` where it is not
/// nillable, and the node named after the parameter where the encoder
/// left it unnamed (`BOGUS`).
pub(crate) fn serialize_zval(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&Sdl>,
    val: Value,
    param: Option<&Param>,
    param_name: &[u8],
    style: Use,
    parent: Node,
) -> Res<Option<Node>> {
    let mut val = val;
    let mut defval: Option<Owner<Value>> = None;
    let enc = param.and_then(|p| p.encode);
    if let Some(p) = param
        && matches!(val.as_ref(), ValueRef::Undef)
        && let Some(element) = p.element.and_then(|e| sdl.and_then(|s| s.ty(e)))
    {
        if let Some(fixed) = &element.fixed {
            let v = PhpString::new(req, fixed)?.into_value();
            val = *v;
            defval = Some(v);
        } else if let Some(def) = &element.def
            && !element.nillable
        {
            let v = PhpString::new(req, def)?.into_value();
            val = *v;
            defval = Some(v);
        }
    }
    let node = master_to_xml(req, cx, enc, val, style, parent);
    if let Some(d) = defval {
        d.give_up(req);
    }
    let node = node?;
    if let Some(n) = node
        && (!n.has_name() || n.name() == b"BOGUS")
    {
        n.set_name(param_name);
    }
    Ok(node)
}

/// The header blocks a server read from a request: `soapHeader`.
pub(crate) struct HeaderCall<'a> {
    pub(crate) function: Option<&'a Function>,
    /// The function the block names, none for one `addSoapHeader()` added.
    pub(crate) function_name: Option<Vec<u8>>,
    pub(crate) must_understand: bool,
    /// The decoded arguments, one owner each.
    pub(crate) parameters: Vec<Owner<Value>>,
    /// What the header's function answered, its one owner.
    pub(crate) retval: Owner<Value>,
    pub(crate) hdr: Option<&'a Header>,
}

/// `serialize_response_call2`: the body of a response, or one header
/// block, by the function's binding or as RPC of `function_name`; the
/// use it wrote with.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn serialize_response_call2(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&Sdl>,
    body: Node,
    function: Option<&Function>,
    function_name: Option<&[u8]>,
    uri: Option<&[u8]>,
    ret: Value,
    version: i64,
    main: bool,
    node: Option<&mut Option<Node>>,
) -> Res<Use> {
    let mut method: Option<Node> = None;
    let style;
    let use_;
    let fnb = soap_binding(sdl, function);
    if let (Some(_), Some(fnb)) = (function, fnb) {
        style = fnb.style;
        use_ = fnb.output.use_;
        if style == Style::Rpc {
            let ns = add_ns(req, body, fnb.output.ns.as_deref());
            if let Some(f) = function {
                if let Some(rn) = &f.response_name {
                    method = body.new_child(ns, rn, None);
                } else if f.response_parameters.is_some() {
                    method = body.new_child(ns, &f.function_name, None);
                }
            }
        }
    } else {
        style = if main { Style::Rpc } else { Style::Document };
        use_ = if main { Use::Encoded } else { Use::Literal };
        if style == Style::Rpc {
            let ns = add_ns(req, body, uri);
            method = body.new_child(ns, function_name.unwrap_or_default(), None);
        }
    }
    let param_count = match function {
        Some(f) => f.response_parameters.as_ref().map_or(0, Table::len),
        None => 1,
    };
    if param_count == 1 {
        let parameter = get_param(function, None, 0, true);
        if style == Style::Rpc {
            let Some(m) = method else {
                return Err(soap_error("Encoding: no method to write the response into"));
            };
            if main && version == SOAP_1_2 {
                let rpc_ns = Ns::new(
                    Some(body),
                    Some(RPC_SOAP12_NAMESPACE),
                    Some(RPC_SOAP12_NS_PREFIX),
                );
                let rpc_result = m.new_child(rpc_ns, b"result", None);
                let param =
                    serialize_parameter(req, cx, sdl, parameter, ret, 0, Some(b"return"), use_, m)?;
                if let (Some(r), Some(p)) = (rpc_result, param) {
                    r.set_content(&p.name());
                }
            } else {
                serialize_parameter(req, cx, sdl, parameter, ret, 0, Some(b"return"), use_, m)?;
            }
        } else {
            let param =
                serialize_parameter(req, cx, sdl, parameter, ret, 0, Some(b"return"), use_, body)?;
            if let Some(param) = param {
                if fnb.is_some() {
                    if let Some(element) =
                        parameter.and_then(|p| p.element).and_then(|e| sdl?.ty(e))
                    {
                        let ns = add_ns(req, param, element.namens.as_deref());
                        param.set_name(element.name.as_deref().unwrap_or_default());
                        param.set_ns_call(ns);
                    }
                } else if param.name() == b"return" {
                    let ns = add_ns(req, param, uri);
                    param.set_name(function_name.unwrap_or_default());
                    param.set_ns_call(ns);
                }
            }
        }
    } else if param_count > 1
        && let ValueRef::Array(arr) = ret.deref().as_ref()
    {
        // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(ret), param_index, param_name,
        // data)` (`Z/ext/soap/soap.c:3587`), no owner of the table taken:
        // the return value holds it while an encoder runs a type map's
        // `to_xml` or `__toString`.
        let mut i = 0u32;
        // A throwable ends the walk as one; a SOAP error ends it with the
        // error, raised after.
        let carried = |error: crate::Error| match error {
            crate::Error::Thrown(thrown) => Err(thrown),
            other @ crate::Error::Fatal { .. } => Ok(ControlFlow::Break(other)),
        };
        let failed = walk::foreach(
            req,
            arr,
            Hold::None,
            |req, row| -> walk::Step<crate::Error> {
                let key = row.key().clone();
                let data = row.value().deref();
                let (pname, pindex): (Option<Vec<u8>>, u64) = match &key {
                    PhpKey::Str(s) => (Some(s.as_bytes().to_vec()), u64::from(i)),
                    PhpKey::Int(n) => (None, u64::try_from(*n).unwrap_or(u64::MAX)),
                };
                let parameter = get_param(function, pname.as_deref(), pindex, true);
                if style == Style::Rpc {
                    if let Some(m) = method {
                        let written = serialize_parameter(
                            req,
                            cx,
                            sdl,
                            parameter,
                            data,
                            i,
                            pname.as_deref(),
                            use_,
                            m,
                        );
                        if let Err(error) = written {
                            return carried(error);
                        }
                    }
                } else {
                    let param = match serialize_parameter(
                        req,
                        cx,
                        sdl,
                        parameter,
                        data,
                        i,
                        pname.as_deref(),
                        use_,
                        body,
                    ) {
                        Ok(param) => param,
                        Err(error) => return carried(error),
                    };
                    if fnb.is_some()
                        && let Some(param) = param
                        && let Some(element) =
                            parameter.and_then(|p| p.element).and_then(|e| sdl?.ty(e))
                    {
                        let ns = add_ns(req, param, element.namens.as_deref());
                        param.set_name(element.name.as_deref().unwrap_or_default());
                        param.set_ns_call(ns);
                    }
                }
                i += 1;
                Ok(ControlFlow::Continue(()))
            },
        )?;
        if let Some(error) = failed {
            return Err(error);
        }
    }
    if use_ == Use::Encoded
        && version == SOAP_1_2
        && let Some(m) = method
    {
        m.set_ns_prop(body.ns(), b"encodingStyle", SOAP_1_2_ENC_NAMESPACE);
    }
    if let Some(node) = node {
        *node = method;
    }
    Ok(use_)
}

/// A fresh document, `xmlNewDoc("1.0")` with php's `UTF-8` encoding, and
/// its envelope in the version's namespace.
fn new_envelope(version: i64) -> Res<(Node, Node, Option<Ns>)> {
    let Some(doc) = Node::new_doc(Some(b"1.0")) else {
        return Err(soap_error("Unknown SOAP version"));
    };
    doc.set_doc_string(php_ext_libxml::ffi::DocString::Encoding, Some(b"UTF-8"));
    let (href, prefix) = if version == SOAP_1_1 {
        (SOAP_1_1_ENV_NAMESPACE, SOAP_1_1_ENV_NS_PREFIX)
    } else if version == SOAP_1_2 {
        (SOAP_1_2_ENV_NAMESPACE, SOAP_1_2_ENV_NS_PREFIX)
    } else {
        doc.free_document();
        return Err(soap_error("Unknown SOAP version"));
    };
    let Some(envelope) = Node::new_element(Some(doc), None, b"Envelope", None) else {
        doc.free_document();
        return Err(soap_error("Unknown SOAP version"));
    };
    let ns = envelope.new_ns(href, Some(prefix));
    envelope.set_ns_call(ns);
    doc.set_root(envelope);
    Ok((doc, envelope, ns))
}

/// The namespaces and `encodingStyle` an encoded message declares on its
/// envelope.
fn declare_encoded(envelope: Node, version: i64, method: Option<Node>) {
    envelope.new_ns(XSD_NAMESPACE, Some(XSD_NS_PREFIX));
    if version == SOAP_1_1 {
        envelope.new_ns(SOAP_1_1_ENC_NAMESPACE, Some(SOAP_1_1_ENC_NS_PREFIX));
        envelope.set_ns_prop(envelope.ns(), b"encodingStyle", SOAP_1_1_ENC_NAMESPACE);
    } else if version == SOAP_1_2 {
        envelope.new_ns(SOAP_1_2_ENC_NAMESPACE, Some(SOAP_1_2_ENC_NS_PREFIX));
        if let Some(m) = method {
            m.set_ns_prop(envelope.ns(), b"encodingStyle", SOAP_1_2_ENC_NAMESPACE);
        }
    }
}

/// `serialize_response_call`: the response a server answers with, or the
/// fault, `function_name` and `uri` naming the RPC element where no
/// description does; none for a one-way operation with nothing to say.
pub(crate) fn serialize_response_call(
    req: &mut Request,
    sdl: Option<&Sdl>,
    function: Option<&Function>,
    function_name: Option<&[u8]>,
    uri: Option<&[u8]>,
    ret: Value,
    headers: &[HeaderCall<'_>],
    version: i64,
) -> Res<Option<Node>> {
    let mut cx = Cx::default();
    encode_reset_ns(req, &mut cx);
    let built = serialize_response_body(
        req,
        &mut cx,
        sdl,
        function,
        function_name,
        uri,
        ret,
        headers,
        version,
    );
    encode_finish(req, &mut cx);
    let built = match req.state::<crate::State>().pending.take() {
        Some(t) => {
            if let Ok((doc, ..)) = &built {
                doc.free_document();
            }
            Err(crate::Error::Thrown(t))
        }
        None => built,
    };
    let (doc, body, head) = built?;
    if let Some(f) = function
        && f.response_name.is_none()
        && body.children().is_none()
        && head.is_none()
    {
        doc.free_document();
        return Ok(None);
    }
    Ok(Some(doc))
}

#[expect(clippy::too_many_lines, reason = "the body of one C function")]
fn serialize_response_body(
    req: &mut Request,
    cx: &mut Cx,
    sdl: Option<&Sdl>,
    function: Option<&Function>,
    function_name: Option<&[u8]>,
    uri: Option<&[u8]>,
    ret: Value,
    headers: &[HeaderCall<'_>],
    version: i64,
) -> Res<(Node, Node, Option<Node>)> {
    let ce = Throwables::of(req)?;
    let (doc, envelope, ns) = new_envelope(version)?;
    let fail = |e| {
        doc.free_document();
        e
    };
    let mut use_ = Use::Literal;
    let mut head: Option<Node> = None;
    let ret = ret.deref();
    let fault_obj = match ret.as_ref() {
        ValueRef::Object(o) if o.instance_of(req, ce.soap_fault) => Some(o),
        _ => None,
    };
    let body;
    if let Some(fault) = fault_obj {
        let headerfault = fault_slot(fault, FaultSlot::HeaderFault);
        if let Some(first) = headers.first()
            && !is_null(&headerfault)
        {
            let mut hdr_enc: Option<EncRef> = None;
            let mut hdr_use = Use::Literal;
            let mut hdr_ret = Some(headerfault);
            let mut hdr_ns = first.hdr.and_then(|h| h.ns.clone());
            let mut hdr_name = first.function_name.clone();
            head = envelope.new_child(ns, b"Header", None);
            if let ValueRef::Object(h) = headerfault.as_ref()
                && instance_of(req, h, "SoapHeader")
            {
                let mut key = Vec::new();
                if let Some(n) = slot_str(h, HEADER_NAMESPACE) {
                    key.extend_from_slice(&n);
                    key.push(b':');
                    hdr_ns = Some(n);
                }
                if let Some(n) = slot_str(h, HEADER_NAME) {
                    key.extend_from_slice(&n);
                    hdr_name = Some(n);
                }
                if let Some(found) = first
                    .hdr
                    .and_then(|hd| hd.headerfaults.as_ref())
                    .and_then(|hf| hf.get_str(&key))
                {
                    hdr_enc = found.encode;
                    hdr_use = found.use_;
                }
                let data = slot(h, HEADER_DATA);
                hdr_ret = (!is_null(&data)).then_some(data);
            }
            let Some(head_node) = head else {
                return Err(fail(soap_error("Encoding: no header")));
            };
            if let Some(hf) = first.function {
                let used = serialize_response_call2(
                    req,
                    cx,
                    sdl,
                    head_node,
                    Some(hf),
                    first.function_name.as_deref(),
                    uri,
                    hdr_ret.unwrap_or_else(Value::unpassed),
                    version,
                    false,
                    None,
                )
                .map_err(fail)?;
                // php sets `use` from the header here, and the fault below
                // sets it again before it is read.
                let _ = used;
            } else {
                let xml_hdr = master_to_xml(
                    req,
                    cx,
                    hdr_enc,
                    hdr_ret.unwrap_or_else(Value::unpassed),
                    hdr_use,
                    head_node,
                )
                .map_err(fail)?;
                if let Some(x) = xml_hdr {
                    if let Some(n) = &hdr_name {
                        x.set_name(n);
                    }
                    if let Some(n) = &hdr_ns {
                        let nsptr = encode_add_ns(req, x, n);
                        x.set_ns_call(nsptr);
                    }
                }
            }
        }
        let Some(b) = envelope.new_child(ns, b"Body", None) else {
            return Err(fail(soap_error("Encoding: no body")));
        };
        body = b;
        let Some(param) = body.new_child(ns, b"Fault", None) else {
            return Err(fail(soap_error("Encoding: no fault")));
        };
        let mut fault_ns = fault_str_of(fault, FaultSlot::CodeNs);
        use_ = Use::Literal;
        let mut sdl_fault: Option<&model::Fault> = None;
        let name = fault_slot(fault, FaultSlot::Name);
        let soap_bound = is_soap_bound(sdl, function);
        if let Some(n) = name.as_str_bytes() {
            if let Some(found) = function
                .and_then(|f| f.faults.as_ref())
                .and_then(|faults| faults.get_str(n))
            {
                sdl_fault = Some(found);
                if soap_bound && let Some(fb) = &found.binding {
                    use_ = fb.use_;
                    if fault_ns.is_none() {
                        fault_ns.clone_from(&fb.ns);
                    }
                }
            }
        } else if let Some(faults) = function.and_then(|f| f.faults.as_ref())
            && faults.len() == 1
            && let Some(found) = faults.first()
        {
            sdl_fault = Some(found);
            if soap_bound && let Some(fb) = &found.binding {
                use_ = fb.use_;
                if fault_ns.is_none() {
                    fault_ns.clone_from(&fb.ns);
                }
            }
        }
        if fault_ns.is_none()
            && let Some(details) = sdl_fault.and_then(|f| f.details.as_ref())
            && details.len() == 1
            && let Some(sparam) = details.first()
            && let Some(element) = sparam.element.and_then(|e| sdl?.ty(e))
        {
            fault_ns.clone_from(&element.namens);
        }
        let detail_name: &[u8] = if version == SOAP_1_1 {
            b"detail"
        } else {
            b"env:Detail"
        };
        if version == SOAP_1_1 {
            if let Some(code) = fault_str_of(fault, FaultSlot::Code) {
                let str = php_ext_standard::escape_html_entities(req, &code, false, 0, None, true)
                    .map_err(fail_thrown(doc))?;
                if let Some(node) = Node::new_element(Some(doc), None, b"faultcode", None) {
                    param.add_child(node);
                    if let Some(fns) = &fault_ns {
                        let nsptr = encode_add_ns(req, node, fns);
                        let prefix = nsptr.and_then(Ns::prefix);
                        node.set_content(&build_qname(str.as_bytes(), prefix.as_deref()));
                    } else {
                        node.set_content_len(str.as_bytes());
                    }
                }
                str.give_up(req);
            }
            let fs = fault_slot(fault, FaultSlot::String);
            if fs.as_str_bytes().is_some() {
                let node = master_to_xml(
                    req,
                    cx,
                    model::get_conversion(model::ty::IS_STRING),
                    fs,
                    Use::Literal,
                    param,
                )
                .map_err(fail)?;
                if let Some(n) = node {
                    n.set_name(b"faultstring");
                }
            }
            let fa = fault_slot(fault, FaultSlot::Actor);
            if fa.as_str_bytes().is_some() {
                let node = master_to_xml(
                    req,
                    cx,
                    model::get_conversion(model::ty::IS_STRING),
                    fa,
                    Use::Literal,
                    param,
                )
                .map_err(fail)?;
                if let Some(n) = node {
                    n.set_name(b"faultactor");
                }
            }
        } else {
            if let Some(code) = fault_str_of(fault, FaultSlot::Code) {
                let str = php_ext_standard::escape_html_entities(req, &code, false, 0, None, true)
                    .map_err(fail_thrown(doc))?;
                if let Some(node) = param
                    .new_child(ns, b"Code", None)
                    .and_then(|c| c.new_child(ns, b"Value", None))
                {
                    if let Some(fns) = &fault_ns {
                        let nsptr = encode_add_ns(req, node, fns);
                        let prefix = nsptr.and_then(Ns::prefix);
                        node.set_content(&build_qname(str.as_bytes(), prefix.as_deref()));
                    } else {
                        node.set_content_len(str.as_bytes());
                    }
                }
                str.give_up(req);
            }
            let fs = fault_slot(fault, FaultSlot::String);
            if fs.as_str_bytes().is_some()
                && let Some(reason) = param.new_child(ns, b"Reason", None)
            {
                let node = master_to_xml(
                    req,
                    cx,
                    model::get_conversion(model::ty::IS_STRING),
                    fs,
                    Use::Literal,
                    reason,
                )
                .map_err(fail)?;
                if let Some(n) = node {
                    n.set_name(b"Text");
                    n.set_ns_call(ns);
                    let lang = fault_str_of(fault, FaultSlot::Lang).unwrap_or_default();
                    set_lang(n, &lang);
                }
            }
        }
        if let Some(details) = sdl_fault.and_then(|f| f.details.as_ref())
            && details.len() == 1
            && let Some(sparam) = details.first()
        {
            let d = fault_slot(fault, FaultSlot::Detail);
            let mut detail = (!is_null(&d)).then_some(d);
            if let Some(node) = Node::new_element(Some(doc), None, detail_name, None) {
                param.add_child(node);
                let element = sparam.element.and_then(|e| sdl?.ty(e));
                if let (Some(dv), Some(element)) = (detail, element)
                    && let ValueRef::Object(obj) = dv.as_ref()
                {
                    let props = obj.props(req).map_err(fail_thrown(doc))?;
                    if props.len() == 1
                        && let Some(name) = &element.name
                        && let Some((_, v)) = props.iter().find(
                            |(k, _)| matches!(k, PhpKey::Str(k) if k.as_bytes() == name.as_slice()),
                        )
                    {
                        detail = Some(*v);
                    }
                }
                let x = serialize_parameter(
                    req,
                    cx,
                    sdl,
                    Some(sparam),
                    detail.unwrap_or_else(Value::unpassed),
                    1,
                    None,
                    use_,
                    node,
                )
                .map_err(fail)?;
                if let Some(x) = x {
                    if let Some(fnb) = soap_binding(sdl, function) {
                        if fnb.style == Style::Rpc && element.is_none() {
                            if let Some(fb_ns) = sdl_fault
                                .and_then(|f| f.binding.as_ref())
                                .and_then(|b| b.ns.as_deref())
                            {
                                let nsx = encode_add_ns(req, x, fb_ns);
                                x.set_ns_call(nsx);
                            }
                        } else if let Some(element) = element {
                            let nsx = add_ns(req, x, element.namens.as_deref());
                            x.set_name(element.name.as_deref().unwrap_or_default());
                            x.set_ns_call(nsx);
                        }
                    }
                    if use_ == Use::Encoded && version == SOAP_1_2 {
                        x.set_ns_prop(envelope.ns(), b"encodingStyle", SOAP_1_2_ENC_NAMESPACE);
                    }
                }
            }
        } else {
            let d = fault_slot(fault, FaultSlot::Detail);
            if !is_null(&d) {
                serialize_zval(req, cx, sdl, d, None, detail_name, use_, param).map_err(fail)?;
            }
        }
    } else {
        if !headers.is_empty() {
            head = envelope.new_child(ns, b"Header", None);
            let Some(head_node) = head else {
                return Err(fail(soap_error("Encoding: no header")));
            };
            for h in headers {
                if matches!(h.retval.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
                    continue;
                }
                let mut hdr_enc: Option<EncRef> = None;
                let mut hdr_use = Use::Literal;
                let mut hdr_ret = Some(h.retval.deref());
                let mut hdr_ns = h.hdr.and_then(|x| x.ns.clone());
                let mut hdr_name = h.function_name.clone();
                let header_object = match h.retval.deref().as_ref() {
                    ValueRef::Object(o) if instance_of(req, o, "SoapHeader") => Some(o),
                    _ => None,
                };
                if let Some(ho) = header_object {
                    let mut key = Vec::new();
                    if let Some(n) = slot_str(ho, HEADER_NAMESPACE) {
                        key.extend_from_slice(&n);
                        key.push(b':');
                        hdr_ns = Some(n);
                    }
                    if let Some(n) = slot_str(ho, HEADER_NAME) {
                        key.extend_from_slice(&n);
                        hdr_name = Some(n);
                    }
                    if let Some(found) = soap_binding(sdl, function)
                        .and_then(|fnb| fnb.output.headers.as_ref())
                        .and_then(|hs| hs.get_str(&key))
                    {
                        hdr_enc = found.encode;
                        hdr_use = found.use_;
                    }
                    let data = slot(ho, HEADER_DATA);
                    hdr_ret = (!is_null(&data)).then_some(data);
                }
                if let Some(hf) = h.function {
                    let mut xml_hdr: Option<Node> = None;
                    let used = serialize_response_call2(
                        req,
                        cx,
                        sdl,
                        head_node,
                        Some(hf),
                        h.function_name.as_deref(),
                        uri,
                        hdr_ret.unwrap_or_else(Value::unpassed),
                        version,
                        false,
                        Some(&mut xml_hdr),
                    )
                    .map_err(fail)?;
                    if used == Use::Encoded {
                        use_ = Use::Encoded;
                    }
                    if let (Some(ho), Some(x)) = (header_object, xml_hdr) {
                        set_soap_header_attributes(x, ho, version);
                    }
                } else {
                    let xml_hdr = master_to_xml(
                        req,
                        cx,
                        hdr_enc,
                        hdr_ret.unwrap_or_else(Value::unpassed),
                        hdr_use,
                        head_node,
                    )
                    .map_err(fail)?;
                    if let Some(x) = xml_hdr {
                        if let Some(n) = &hdr_name {
                            x.set_name(n);
                        }
                        if let Some(n) = &hdr_ns {
                            let nsptr = encode_add_ns(req, x, n);
                            x.set_ns_call(nsptr);
                        }
                        if let Some(ho) = header_object {
                            set_soap_header_attributes(x, ho, version);
                        }
                    }
                }
            }
            if head_node.children().is_none() {
                head_node.unlink();
                head_node.free();
                head = None;
            }
        }
        let Some(b) = envelope.new_child(ns, b"Body", None) else {
            return Err(fail(soap_error("Encoding: no body")));
        };
        body = b;
        let used = serialize_response_call2(
            req,
            cx,
            sdl,
            body,
            function,
            function_name,
            uri,
            ret,
            version,
            true,
            None,
        )
        .map_err(fail)?;
        if used == Use::Encoded {
            use_ = Use::Encoded;
        }
    }
    if use_ == Use::Encoded {
        envelope.new_ns(XSD_NAMESPACE, Some(XSD_NS_PREFIX));
        if version == SOAP_1_1 {
            envelope.new_ns(SOAP_1_1_ENC_NAMESPACE, Some(SOAP_1_1_ENC_NS_PREFIX));
            envelope.set_ns_prop(envelope.ns(), b"encodingStyle", SOAP_1_1_ENC_NAMESPACE);
        } else if version == SOAP_1_2 {
            envelope.new_ns(SOAP_1_2_ENC_NAMESPACE, Some(SOAP_1_2_ENC_NS_PREFIX));
        }
    }
    Ok((doc, body, head))
}

/// A throwable leaving the response's build: the document freed first.
fn fail_thrown(doc: Node) -> impl Fn(Thrown) -> crate::Error {
    move |t| {
        doc.free_document();
        crate::Error::Thrown(t)
    }
}

/// A string slot's bytes.
fn slot_str(object: PhpObject, index: u32) -> Option<Vec<u8>> {
    slot(object, index).as_str_bytes().map(<[u8]>::to_vec)
}

/// A fault slot's bytes, where it holds a string.
fn fault_str_of(object: PhpObject, which: FaultSlot) -> Option<Vec<u8>> {
    fault_slot(object, which).as_str_bytes().map(<[u8]>::to_vec)
}

/// `serialize_function_call`: the request a client sends.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
pub(crate) fn serialize_function_call(
    req: &mut Request,
    this: PhpObject,
    sdl: Option<&Sdl>,
    function: Option<&Function>,
    function_name: Option<&[u8]>,
    uri: Option<&[u8]>,
    arguments: &[Value],
    version: i64,
    soap_headers: Option<&[Value]>,
) -> Res<Node> {
    let mut cx = Cx::default();
    encode_reset_ns(req, &mut cx);
    let (doc, envelope, ns) = match new_envelope(version) {
        Ok(made) => made,
        Err(e) => {
            encode_finish(req, &mut cx);
            return Err(e);
        }
    };
    let built = (|| -> Res<()> {
        let head = soap_headers.and_then(|_| envelope.new_child(ns, b"Header", None));
        let Some(body) = envelope.new_child(ns, b"Body", None) else {
            return Err(soap_error("Unknown SOAP version"));
        };
        let mut method: Option<Node> = None;
        let mut hdrs: Option<&Table<Header>> = None;
        let style;
        let mut use_;
        if let Some(fnb) = soap_binding(sdl, function) {
            hdrs = fnb.input.headers.as_ref();
            style = fnb.style;
            use_ = fnb.input.use_;
            if style == Style::Rpc {
                let nsx = add_ns(req, body, fnb.input.ns.as_deref());
                if let Some(f) = function {
                    let name = f.request_name.as_deref().unwrap_or(&f.function_name);
                    method = body.new_child(nsx, name, None);
                }
            }
        } else {
            let zstyle = client_slot(this, ClientSlot::Style);
            style = match zstyle.as_ref() {
                ValueRef::Int(2) => Style::Document,
                _ => Style::Rpc,
            };
            if style == Style::Rpc {
                let nsx = add_ns(req, body, uri);
                method = if let Some(n) = function_name {
                    body.new_child(nsx, n, None)
                } else if let Some(n) = function.and_then(|f| f.request_name.as_deref()) {
                    body.new_child(nsx, n, None)
                } else if let Some(f) = function {
                    body.new_child(nsx, &f.function_name, None)
                } else {
                    Some(body)
                };
            } else {
                method = Some(body);
            }
            use_ = match client_slot(this, ClientSlot::Use).as_ref() {
                ValueRef::Int(2) => Use::Literal,
                _ => Use::Encoded,
            };
        }
        let soap_bound = soap_binding(sdl, function).is_some();
        let count = function
            .and_then(|f| f.request_parameters.as_ref())
            .map_or(0, Table::len);
        let total = arguments.len().max(count);
        for i in 0..total {
            let at = u32::try_from(i).unwrap_or(u32::MAX);
            let parameter = get_param(function, None, u64::from(at), false);
            let value = arguments.get(i).copied().unwrap_or_else(Value::unpassed);
            if i >= arguments.len()
                && function
                    .and_then(|f| f.request_parameters.as_ref())
                    .is_none()
            {
                break;
            }
            if style == Style::Rpc {
                if let Some(m) = method {
                    serialize_parameter(req, &mut cx, sdl, parameter, value, at, None, use_, m)?;
                }
            } else {
                let param =
                    serialize_parameter(req, &mut cx, sdl, parameter, value, at, None, use_, body)?;
                if soap_bound
                    && let Some(param) = param
                    && let Some(element) =
                        parameter.and_then(|p| p.element).and_then(|e| sdl?.ty(e))
                {
                    let nsx = add_ns(req, param, element.namens.as_deref());
                    param.set_name(element.name.as_deref().unwrap_or_default());
                    param.set_ns_call(nsx);
                }
            }
        }
        if let (Some(head), Some(headers)) = (head, soap_headers) {
            for header in headers {
                let header = header.deref();
                let Some(h) = header.as_object() else {
                    continue;
                };
                if !instance_of(req, h, "SoapHeader") {
                    continue;
                }
                let (Some(name), Some(hns)) =
                    (slot_str(h, HEADER_NAME), slot_str(h, HEADER_NAMESPACE))
                else {
                    continue;
                };
                let mut hdr_use = Use::Literal;
                let mut enc: Option<EncRef> = None;
                if let Some(hdrs) = hdrs {
                    let mut key = hns.clone();
                    key.push(b':');
                    key.extend_from_slice(&name);
                    if let Some(found) = hdrs.get_str(&key) {
                        hdr_use = found.use_;
                        enc = found.encode;
                        if hdr_use == Use::Encoded {
                            use_ = Use::Encoded;
                        }
                    }
                }
                let data = slot(h, HEADER_DATA);
                let hnode = if is_null(&data) {
                    let made = Node::new_element(Some(doc), None, &name, None);
                    if let Some(m) = made {
                        head.add_child(m);
                    }
                    made
                } else {
                    let made = master_to_xml(req, &mut cx, enc, data, hdr_use, head)?;
                    if let Some(m) = made {
                        m.set_name(&name);
                    }
                    made
                };
                if let Some(hn) = hnode {
                    let nsptr = encode_add_ns(req, hn, &hns);
                    hn.set_ns_call(nsptr);
                    set_soap_header_attributes(hn, h, version);
                }
            }
        }
        if use_ == Use::Encoded {
            declare_encoded(envelope, version, method);
        }
        Ok(())
    })();
    encode_finish(req, &mut cx);
    let built = match req.state::<crate::State>().pending.take() {
        Some(t) => Err(crate::Error::Thrown(t)),
        None => built,
    };
    match built {
        Ok(()) => Ok(doc),
        Err(e) => {
            doc.free_document();
            Err(e)
        }
    }
}

// ---- __getFunctions() and __getTypes()

/// The `type_str` of an encoder, or `UNKNOWN`.
fn type_or_unknown(sdl: &Sdl, enc: Option<EncRef>) -> Vec<u8> {
    type_str(Some(sdl), enc).unwrap_or_else(|| b"UNKNOWN".to_vec())
}

/// `function_to_string`.
pub(crate) fn function_to_string(sdl: &Sdl, function: &Function) -> Vec<u8> {
    let mut buf = Vec::new();
    match &function.response_parameters {
        Some(rp) if !rp.is_empty() => {
            if rp.len() == 1 {
                if let Some(t) = rp.first().and_then(|p| type_str(Some(sdl), p.encode)) {
                    buf.extend_from_slice(&t);
                    buf.push(b' ');
                } else {
                    buf.extend_from_slice(b"UNKNOWN ");
                }
            } else {
                buf.extend_from_slice(b"list(");
                for (i, p) in rp.values().enumerate() {
                    if i > 0 {
                        buf.extend_from_slice(b", ");
                    }
                    buf.extend_from_slice(&type_or_unknown(sdl, p.encode));
                    buf.extend_from_slice(b" $");
                    buf.extend_from_slice(p.param_name.as_deref().unwrap_or_default());
                }
                buf.extend_from_slice(b") ");
            }
        }
        _ => buf.extend_from_slice(b"void "),
    }
    buf.extend_from_slice(&function.function_name);
    buf.push(b'(');
    if let Some(rp) = &function.request_parameters {
        for (i, p) in rp.values().enumerate() {
            if i > 0 {
                buf.extend_from_slice(b", ");
            }
            buf.extend_from_slice(&type_or_unknown(sdl, p.encode));
            buf.extend_from_slice(b" $");
            buf.extend_from_slice(p.param_name.as_deref().unwrap_or_default());
        }
    }
    buf.push(b')');
    buf
}

/// `model_to_string`.
fn model_to_string(sdl: &Sdl, model: &ContentModel, buf: &mut Vec<u8>, level: usize) {
    match (&model.kind, &model.u) {
        (ContentKind::Element, Content::Element(e)) => {
            if let Some(t) = sdl.ty(*e) {
                type_to_string(sdl, t, buf, level);
            }
            buf.extend_from_slice(b";\n");
        }
        (ContentKind::Any, _) => {
            buf.extend(std::iter::repeat_n(b' ', level));
            buf.extend_from_slice(b"<anyXML> any;\n");
        }
        (
            ContentKind::Sequence | ContentKind::All | ContentKind::Choice,
            Content::Particles(list),
        ) => {
            for m in list {
                model_to_string(sdl, m, buf, level);
            }
        }
        (ContentKind::Group, Content::Group(g)) => {
            if let Some(m) = sdl.ty(*g).and_then(|t| t.model.as_deref()) {
                model_to_string(sdl, m, buf, level);
            }
        }
        _ => {}
    }
}

/// An attribute's extra attribute `WSDL_NAMESPACE:name`'s value.
fn extra(t: &Type, attr: &[u8], ext: &[u8]) -> Option<Vec<u8>> {
    let a = t.attributes.as_ref()?.get_str(attr)?;
    let mut key = WSDL_NAMESPACE.to_vec();
    key.push(b':');
    key.extend_from_slice(ext);
    a.extra_attributes.as_ref()?.get_str(&key)?.val.clone()
}

/// `type_to_string`.
#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
pub(crate) fn type_to_string(sdl: &Sdl, t: &Type, buf: &mut Vec<u8>, level: usize) {
    let spaces = vec![b' '; level];
    buf.extend_from_slice(&spaces);
    let name = t.name.as_deref().unwrap_or_default();
    let enc = t
        .encode
        .and_then(|e| crate::sdl::encoder(Some(sdl), None, e));
    match t.kind {
        TypeKind::Simple => {
            if let Some(e) = &enc {
                buf.extend_from_slice(e.details.type_str.as_deref().unwrap_or_default());
                buf.push(b' ');
            } else {
                buf.extend_from_slice(b"anyType ");
            }
            buf.extend_from_slice(name);
            if let Some(en) = t.restrictions.as_ref().and_then(|r| r.enumeration.as_ref()) {
                buf.extend_from_slice(b" {");
                let mut first = true;
                for (k, _) in en.iter() {
                    if let Key::Str(k) = k {
                        if !first {
                            buf.extend_from_slice(b", ");
                        }
                        first = false;
                        buf.extend_from_slice(k);
                    }
                }
                buf.push(b'}');
            }
        }
        TypeKind::List => {
            buf.extend_from_slice(b"list ");
            buf.extend_from_slice(name);
            if let Some(els) = &t.elements {
                buf.extend_from_slice(b" {");
                for e in els.values() {
                    if let Some(item) = sdl.ty(*e) {
                        buf.extend_from_slice(item.name.as_deref().unwrap_or_default());
                    }
                }
                buf.push(b'}');
            }
        }
        TypeKind::Union => {
            buf.extend_from_slice(b"union ");
            buf.extend_from_slice(name);
            if let Some(els) = &t.elements {
                buf.extend_from_slice(b" {");
                for e in els.values() {
                    // php's `first` never leaves 0: a comma before every
                    // member.
                    buf.push(b',');
                    if let Some(item) = sdl.ty(*e) {
                        buf.extend_from_slice(item.name.as_deref().unwrap_or_default());
                    }
                }
                buf.push(b'}');
            }
        }
        TypeKind::Complex | TypeKind::Restriction | TypeKind::Extension => {
            let is_array = enc.as_ref().is_some_and(|e| {
                e.details.type_ == model::ty::IS_ARRAY
                    || e.details.type_ == model::ty::SOAP_ENC_ARRAY
            });
            if is_array {
                let mut array_type_attr = SOAP_1_1_ENC_NAMESPACE.to_vec();
                array_type_attr.extend_from_slice(b":arrayType");
                if let Some(val) = extra(t, &array_type_attr, b"arrayType") {
                    let end = val.iter().position(|c| *c == b'[');
                    let len = end.unwrap_or(val.len());
                    if len == 0 {
                        buf.extend_from_slice(b"anyType");
                    } else {
                        buf.extend_from_slice(&val[..len]);
                    }
                    buf.push(b' ');
                    buf.extend_from_slice(name);
                    if let Some(end) = end {
                        buf.extend_from_slice(&val[end..]);
                    }
                } else {
                    let mut item_type_attr = SOAP_1_2_ENC_NAMESPACE.to_vec();
                    item_type_attr.extend_from_slice(b":itemType");
                    let single = t
                        .elements
                        .as_ref()
                        .filter(|e| e.len() == 1)
                        .and_then(|e| e.first())
                        .and_then(|id| sdl.ty(*id))
                        .and_then(|el| type_str(Some(sdl), el.encode));
                    if let Some(val) = extra(t, &item_type_attr, b"itemType") {
                        buf.extend_from_slice(&val);
                        buf.push(b' ');
                    } else if let Some(ts) = single {
                        buf.extend_from_slice(&ts);
                        buf.push(b' ');
                    } else {
                        buf.extend_from_slice(b"anyType ");
                    }
                    buf.extend_from_slice(name);
                    let mut size_attr = SOAP_1_2_ENC_NAMESPACE.to_vec();
                    size_attr.extend_from_slice(b":arraySize");
                    if let Some(val) = extra(t, &size_attr, b"itemType") {
                        buf.push(b'[');
                        buf.extend_from_slice(&val);
                        buf.push(b']');
                    } else {
                        buf.extend_from_slice(b"[]");
                    }
                }
            } else {
                buf.extend_from_slice(b"struct ");
                buf.extend_from_slice(name);
                buf.extend_from_slice(b" {\n");
                if matches!(t.kind, TypeKind::Restriction | TypeKind::Extension)
                    && let Some(start) = t.encode
                {
                    let mut cur = Some(start);
                    for _ in 0..1024 {
                        let Some(c) = cur else { break };
                        let Some(e) = crate::sdl::encoder(Some(sdl), None, c) else {
                            break;
                        };
                        let Some(st) = e.details.sdl_type.and_then(|id| sdl.ty(id)) else {
                            break;
                        };
                        if st.encode == Some(c)
                            || matches!(
                                st.kind,
                                TypeKind::Simple | TypeKind::List | TypeKind::Union
                            )
                        {
                            break;
                        }
                        cur = st.encode;
                    }
                    if cur.is_some() {
                        buf.extend_from_slice(&spaces);
                        buf.push(b' ');
                        buf.extend_from_slice(
                            enc.as_ref()
                                .and_then(|e| e.details.type_str.as_deref())
                                .unwrap_or_default(),
                        );
                        buf.extend_from_slice(b" _;\n");
                    }
                }
                if let Some(m) = &t.model {
                    model_to_string(sdl, m, buf, level + 1);
                }
                if let Some(attrs) = &t.attributes {
                    for a in attrs.values() {
                        buf.extend_from_slice(&spaces);
                        buf.push(b' ');
                        if let Some(ts) = type_str(Some(sdl), a.encode) {
                            buf.extend_from_slice(&ts);
                            buf.push(b' ');
                        } else {
                            buf.extend_from_slice(b"UNKNOWN ");
                        }
                        buf.extend_from_slice(a.name.as_deref().unwrap_or_default());
                        buf.extend_from_slice(b";\n");
                    }
                }
                buf.extend_from_slice(&spaces);
                buf.push(b'}');
            }
        }
    }
}

// ---- the typemap option

/// The encoder a typemap entry copies, `enc` and `new_enc` of
/// `soap_create_typemap`: the one the description or the defaults name
/// for `type_ns:type_name`, or `UNKNOWN_TYPE`'s under that name.
fn typemap_encoder(sdl: Option<&Sdl>, type_name: &[u8], type_ns: Option<&[u8]>) -> model::Encoder {
    let found = match type_ns {
        Some(ns) => crate::sdl::get_encoder(sdl, Some(ns), type_name),
        None => crate::sdl::get_encoder_ex(sdl, type_name),
    };
    if let Some(e) = found.and_then(|f| crate::sdl::encoder(sdl, None, f)) {
        return model::Encoder {
            details: model::EncodeType {
                type_: e.details.type_,
                type_str: e.details.type_str.clone(),
                ns: e.details.ns.clone(),
                clark_notation: None,
                sdl_type: e.details.sdl_type,
            },
            to_zval: e.to_zval,
            to_xml: e.to_xml,
        };
    }
    let unknown = model::builtin(0).map(model::Builtin::encoder);
    model::Encoder {
        details: model::EncodeType {
            type_: model::ty::UNKNOWN_TYPE,
            type_str: Some(type_name.to_vec()),
            ns: type_ns.map(<[u8]>::to_vec),
            clark_notation: None,
            sdl_type: None,
        },
        to_zval: unknown.as_ref().map_or(model::ToZval::Guess, |u| u.to_zval),
        to_xml: unknown.as_ref().map_or(model::ToXml::Guess, |u| u.to_xml),
    }
}

/// `soap_create_typemap`: each `type_name`/`type_ns` entry of the option
/// over the encoder it names (or `UNKNOWN_TYPE`'s), its `to_xml` and
/// `from_xml` replaced by the program's, by `ns:type`; none where the
/// option yields none. An entry under a key already made replaces it,
/// whose owners are given up as the table's `delete_encoder` gives them
/// up; where the walk throws, the entries made before are given up,
/// where php's `return NULL` leaves its table to the request's end.
pub(crate) fn soap_create_typemap(
    req: &mut Request,
    sdl: Option<&Sdl>,
    ht: PhpArray,
) -> Result<Option<Typemap>, Thrown> {
    let mut typemap: Option<Typemap> = None;
    // `ZEND_HASH_FOREACH_VAL(ht, tmp)` (`Z/ext/soap/soap.c:814`), no owner
    // of the table taken: the option's array holds it while a `TypeError`
    // reaches the program's handler.
    let walked = walk::foreach(req, ht, Hold::None, |req, row| -> walk::Step {
        let tmp = row.value().deref();
        let ValueRef::Array(entry) = tmp.as_ref() else {
            let given = php_ext::coerce::value_name(req, &tmp);
            return Err(php_ext::raising::type_error(
                req,
                &format!(
                    "SoapHeader::__construct(): \"typemap\" option must be of type array, {given} given"
                ),
            ));
        };
        let mut type_name: Option<Vec<u8>> = None;
        let mut type_ns: Option<Vec<u8>> = None;
        let mut to_xml: Option<Value> = None;
        let mut to_zval: Option<Value> = None;
        if !entry.is_packed() {
            for (k, v) in walk::rows(req, entry) {
                let PhpKey::Str(name) = &k else { continue };
                let v = v.deref();
                match name.as_bytes() {
                    b"type_name" => {
                        if let Some(s) = v.as_str_bytes() {
                            type_name = Some(s.to_vec());
                        }
                    }
                    b"type_ns" => {
                        if let Some(s) = v.as_str_bytes() {
                            type_ns = Some(s.to_vec());
                        }
                    }
                    b"to_xml" => to_xml = Some(v),
                    b"from_xml" => to_zval = Some(v),
                    _ => {}
                }
            }
        }
        let Some(type_name) = type_name else {
            return Ok(ControlFlow::Continue(()));
        };
        let mut new_enc = typemap_encoder(sdl, &type_name, type_ns.as_deref());
        if to_xml.is_some() {
            new_enc.to_xml = model::ToXml::User;
        }
        if to_zval.is_some() {
            new_enc.to_zval = model::ToZval::User;
        }
        let to_xml = to_xml.map(|v| Owner::take(req, v));
        let to_zval = to_zval.map(|v| Owner::take(req, v));
        let mut key = Vec::new();
        if let Some(ns) = &type_ns {
            key.extend_from_slice(ns);
            key.push(b':');
        }
        key.extend_from_slice(&type_name);
        let replaced = typemap.get_or_insert_with(Table::new).update_str(
            &key,
            TypemapEntry {
                enc: new_enc,
                to_xml,
                to_zval,
            },
        );
        if let Some(replaced) = replaced {
            replaced.release(req);
        }
        Ok(ControlFlow::Continue(()))
    });
    match walked {
        Ok(_) => Ok(typemap),
        Err(thrown) => {
            if let Some(typemap) = typemap {
                typemap.release(req);
            }
            Err(thrown)
        }
    }
}

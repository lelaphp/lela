//! `php_packet_soap.c`: the response a client got, parsed into the
//! value its call answers, a fault kept on the client, and the headers
//! `__soapCall()` hands back.

use php_ext::abi::{Ref, Untyped};
use php_ext::coerce::with_string;
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, Value, walk};
use php_ext_libxml::ffi::{Kind, Node};

use crate::encoding::{Cx, encode_finish, encode_reset_ns, master_to_zval};
use crate::message::soap_binding;
use crate::model::{
    EncRef, Function, RPC_SOAP12_NAMESPACE, SOAP_1_1_ENC_NAMESPACE, SOAP_1_1_ENV_NAMESPACE,
    SOAP_1_2_ENV_NAMESPACE, Sdl, Style, XML_NAMESPACE, get_conversion, ty,
};
use crate::shared::add_soap_fault;
use crate::xml::{attr_content, attr_is_equal, get_attribute, get_node, node_is_equal};
use crate::{Res, SOAP_1_1, SOAP_1_2};

/// The fault a malformed response is answered with, the document freed.
fn bad(
    req: &mut Request,
    this: PhpObject,
    doc: Option<Node>,
    code: &[u8],
    text: &[u8],
) -> Res<(bool, Owner<Value>)> {
    if let Some(d) = doc {
        d.free_document();
    }
    add_soap_fault(req, this, code, text, None, None, b"en")?;
    Ok((false, Owner::null()))
}

/// The next element from `trav` on.
fn next_element(mut trav: Option<Node>) -> Option<Node> {
    while let Some(t) = trav {
        if t.kind() == Kind::Element {
            return Some(t);
        }
        trav = t.next();
    }
    None
}

/// `master_to_zval` over a node of the response, with the call's own
/// ref map.
fn decode(req: &mut Request, cx: &mut Cx, enc: Option<EncRef>, node: Node) -> Res<Owner<Value>> {
    master_to_zval(req, cx, enc, node)
}

/// A string decoded and made a string, `convert_to_string`.
fn decode_string(req: &mut Request, cx: &mut Cx, node: Node) -> Res<Vec<u8>> {
    let v = decode(req, cx, get_conversion(ty::IS_STRING), node)?;
    let bytes = with_string(req, &v, <[u8]>::to_vec);
    v.give_up(req);
    Ok(bytes?)
}

/// `parse_packet_soap`: whether the call succeeded, and what it answers.
pub(crate) fn parse_packet_soap(
    req: &mut Request,
    this: PhpObject,
    buffer: &[u8],
    fn_: Option<&Function>,
    sdl: Option<&Sdl>,
    soap_headers: Option<&Ref<Untyped<Value>>>,
) -> Res<(bool, Owner<Value>)> {
    let mut cx = Cx::default();
    encode_reset_ns(req, &mut cx);
    let r = parse(req, &mut cx, this, buffer, fn_, sdl, soap_headers);
    encode_finish(req, &mut cx);
    r
}

#[expect(clippy::too_many_lines, reason = "one C function, kept whole")]
fn parse(
    req: &mut Request,
    cx: &mut Cx,
    this: PhpObject,
    buffer: &[u8],
    fn_: Option<&Function>,
    sdl: Option<&Sdl>,
    soap_headers: Option<&Ref<Untyped<Value>>>,
) -> Res<(bool, Owner<Value>)> {
    if buffer.is_empty() {
        return Ok((true, Owner::null()));
    }
    let Some(response) = crate::xml::parse_memory(req, buffer)? else {
        return bad(
            req,
            this,
            None,
            b"Client",
            b"looks like we got no XML document",
        );
    };
    if response.int_subset().is_some() {
        return bad(
            req,
            this,
            Some(response),
            b"Client",
            b"DTD are not supported by SOAP",
        );
    }
    let mut env = None;
    let mut envelope_ns: &[u8] = b"";
    let mut soap_version = SOAP_1_1;
    let mut trav = response.children();
    while let Some(t) = trav {
        if t.kind() == Kind::Element {
            if env.is_none() && node_is_equal(t, Some(b"Envelope"), Some(SOAP_1_1_ENV_NAMESPACE)) {
                env = Some(t);
                envelope_ns = SOAP_1_1_ENV_NAMESPACE;
                soap_version = SOAP_1_1;
            } else if env.is_none()
                && node_is_equal(t, Some(b"Envelope"), Some(SOAP_1_2_ENV_NAMESPACE))
            {
                env = Some(t);
                envelope_ns = SOAP_1_2_ENV_NAMESPACE;
                soap_version = SOAP_1_2;
            } else {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"VersionMismatch",
                    b"Wrong Version",
                );
            }
        }
        trav = t.next();
    }
    let Some(env) = env else {
        return bad(
            req,
            this,
            Some(response),
            b"Client",
            b"looks like we got XML without \"Envelope\" element",
        );
    };
    let mut attr = env.properties();
    while let Some(a) = attr {
        if a.ns().is_none() {
            return bad(
                req,
                this,
                Some(response),
                b"Client",
                b"A SOAP Envelope element cannot have non Namespace qualified attributes",
            );
        } else if attr_is_equal(a, b"encodingStyle", Some(SOAP_1_2_ENV_NAMESPACE)) {
            if soap_version == SOAP_1_2 {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"encodingStyle cannot be specified on the Envelope",
                );
            } else if attr_content(a) != SOAP_1_1_ENC_NAMESPACE {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"Unknown data encoding style",
                );
            }
        }
        attr = a.next();
    }
    let mut head = None;
    let mut trav = next_element(env.children());
    if let Some(t) = trav
        && node_is_equal(t, Some(b"Header"), Some(envelope_ns))
    {
        head = Some(t);
        trav = t.next();
    }
    let mut body = None;
    trav = next_element(trav);
    if let Some(t) = trav
        && node_is_equal(t, Some(b"Body"), Some(envelope_ns))
    {
        body = Some(t);
        trav = t.next();
    }
    trav = next_element(trav);
    let Some(body) = body else {
        return bad(
            req,
            this,
            Some(response),
            b"Client",
            b"Body must be present in a SOAP envelope",
        );
    };
    let mut attr = body.properties();
    while let Some(a) = attr {
        if a.ns().is_none() {
            if soap_version == SOAP_1_2 {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"A SOAP Body element cannot have non Namespace qualified attributes",
                );
            }
        } else if attr_is_equal(a, b"encodingStyle", Some(SOAP_1_2_ENV_NAMESPACE)) {
            if soap_version == SOAP_1_2 {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"encodingStyle cannot be specified on the Body",
                );
            } else if attr_content(a) != SOAP_1_1_ENC_NAMESPACE {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"Unknown data encoding style",
                );
            }
        }
        attr = a.next();
    }
    if trav.is_some() && soap_version == SOAP_1_2 {
        return bad(
            req,
            this,
            Some(response),
            b"Client",
            b"A SOAP 1.2 envelope can contain only Header and Body",
        );
    }
    if let Some(head) = head {
        let mut attr = head.properties();
        while let Some(a) = attr {
            if a.ns().is_none() {
                return bad(
                    req,
                    this,
                    Some(response),
                    b"Client",
                    b"A SOAP Header element cannot have non Namespace qualified attributes",
                );
            } else if attr_is_equal(a, b"encodingStyle", Some(SOAP_1_2_ENV_NAMESPACE)) {
                if soap_version == SOAP_1_2 {
                    return bad(
                        req,
                        this,
                        Some(response),
                        b"Client",
                        b"encodingStyle cannot be specified on the Header",
                    );
                } else if attr_content(a) != SOAP_1_1_ENC_NAMESPACE {
                    return bad(
                        req,
                        this,
                        Some(response),
                        b"Client",
                        b"Unknown data encoding style",
                    );
                }
            }
            attr = a.next();
        }
    }
    // A fault.
    if let Some(fault) = get_node(body.children(), b"Fault", Some(envelope_ns)) {
        let r = read_fault(req, cx, this, fault, soap_version);
        response.free_document();
        r?;
        return Ok((false, Owner::null()));
    }
    let r = read_body(req, cx, body, head, fn_, sdl, soap_headers);
    response.free_document();
    r.map(|v| (true, v))
}

/// The `<Fault>` of a response kept on the client.
fn read_fault(
    req: &mut Request,
    cx: &mut Cx,
    this: PhpObject,
    fault: Node,
    version: i64,
) -> Res<()> {
    let mut faultcode: Option<Vec<u8>> = None;
    let mut faultstring: Option<Vec<u8>> = None;
    let mut faultactor: Option<Vec<u8>> = None;
    let mut lang: Vec<u8> = Vec::new();
    let mut details: Option<Owner<Value>> = None;
    if version == SOAP_1_1 {
        if let Some(t) = get_node(fault.children(), b"faultcode", None)
            && let Some(c) = t.children()
        {
            faultcode = c.content();
        }
        if let Some(t) = get_node(fault.children(), b"faultstring", None)
            && t.children().is_some()
        {
            faultstring = Some(decode_string(req, cx, t)?);
        }
        if let Some(t) = get_node(fault.children(), b"faultactor", None)
            && t.children().is_some()
        {
            faultactor = Some(decode_string(req, cx, t)?);
        }
        if let Some(t) = get_node(fault.children(), b"detail", None) {
            details = Some(decode(req, cx, None, t)?);
        }
    } else {
        if let Some(t) = get_node(fault.children(), b"Code", None)
            && t.children().is_some()
            && let Some(v) = get_node(t.children(), b"Value", None)
            && let Some(c) = v.children()
        {
            faultcode = c.content();
        }
        if let Some(t) = get_node(fault.children(), b"Reason", None)
            && t.children().is_some()
            && let Some(text) = get_node(t.children(), b"Text", None)
            && text.children().is_some()
        {
            faultstring = Some(decode_string(req, cx, text)?);
            if let Some(a) = get_attribute(text.properties(), b"lang", Some(XML_NAMESPACE))
                && a.children().is_some()
            {
                lang = attr_content(a);
            }
        }
        if let Some(t) = get_node(fault.children(), b"Detail", None) {
            details = Some(decode(req, cx, None, t)?);
        }
    }
    let added = add_soap_fault(
        req,
        this,
        faultcode.as_deref().unwrap_or_default(),
        faultstring.as_deref().unwrap_or_default(),
        faultactor.as_deref(),
        details.as_ref().map(|d| **d),
        &lang,
    );
    if let Some(d) = details {
        d.give_up(req);
    }
    added?;
    Ok(())
}

/// The `<Body>` of a successful response decoded, and its headers.
#[expect(clippy::too_many_lines, reason = "the body of one C function")]
fn read_body(
    req: &mut Request,
    cx: &mut Cx,
    body: Node,
    head: Option<Node>,
    fn_: Option<&Function>,
    sdl: Option<&Sdl>,
    soap_headers: Option<&Ref<Untyped<Value>>>,
) -> Res<Owner<Value>> {
    let mut return_value = PhpArray::new(req)?;
    let mut param_count = 0usize;
    let mut hdrs = None;
    if let Some(resp) = next_element(body.children()) {
        if let (Some(f), Some(fnb)) = (fn_, soap_binding(sdl, fn_)) {
            hdrs = fnb.output.headers.as_ref();
            if let Some(rp) = &f.response_parameters {
                let res_count = rp.len();
                for param in rp.values() {
                    let (name, ns): (Vec<u8>, Option<Vec<u8>>) = if fnb.style == Style::Document {
                        match param.element.and_then(|e| sdl?.ty(e)) {
                            Some(el) => (el.name.clone().unwrap_or_default(), el.namens.clone()),
                            None => (param.param_name.clone().unwrap_or_default(), None),
                        }
                    } else {
                        (f.response_name.clone().unwrap_or_default(), None)
                    };
                    let mut cur = get_node(Some(resp), &name, ns.as_deref());
                    if cur.is_none() {
                        cur = get_node(Some(resp), &name, None);
                    }
                    if cur.is_none() && fnb.style == Style::Rpc {
                        cur = Some(resp);
                    }
                    let mut val = None;
                    if let Some(c) = cur {
                        if fnb.style == Style::Document {
                            val = Some(c);
                        } else {
                            val = get_node(
                                c.children(),
                                param.param_name.as_deref().unwrap_or_default(),
                                None,
                            );
                            if res_count == 1 {
                                if val.is_none() {
                                    val = get_node(c.children(), b"return", None);
                                }
                                if val.is_none() {
                                    val = get_node(c.children(), b"result", None);
                                }
                                if val.is_none()
                                    && let Some(first) = c.children()
                                    && first.next().is_none()
                                {
                                    val = Some(first);
                                }
                            }
                        }
                    }
                    let tmp = match val {
                        None => Owner::null(),
                        Some(v) => match decode(req, cx, param.encode, v) {
                            Ok(tmp) => tmp,
                            Err(err) => {
                                return_value.give_up(req);
                                return Err(err);
                            }
                        },
                    };
                    let name = param.param_name.as_deref().unwrap_or_default();
                    return_value.set_symtable_str(req, name, tmp)?;
                    param_count += 1;
                }
            }
        } else {
            let mut val = resp.children();
            loop {
                val = next_element(val);
                let Some(v) = val else { break };
                if !node_is_equal(v, Some(b"result"), Some(RPC_SOAP12_NAMESPACE)) {
                    let tmp = match decode(req, cx, None, v) {
                        Ok(tmp) => tmp,
                        Err(err) => {
                            return_value.give_up(req);
                            return Err(err);
                        }
                    };
                    if v.has_name() {
                        let name = v.name();
                        if let Some(existing) = return_value.find(&name) {
                            if let Some(arr) = existing.deref().as_array() {
                                let mut arr = Owner::take(req, arr);
                                arr.append(req, tmp)?;
                                return_value.set_str(req, &name, arr.into_value())?;
                            } else {
                                tmp.give_up(req);
                            }
                        } else if v.next().is_some() && get_node(v.next(), &name, None).is_some() {
                            let mut arr = PhpArray::new(req)?;
                            arr.append(req, tmp)?;
                            return_value.set_symtable_str(req, &name, arr.into_value())?;
                        } else {
                            return_value.set_symtable_str(req, &name, tmp)?;
                        }
                    } else {
                        return_value.append(req, tmp)?;
                    }
                    param_count += 1;
                }
                val = v.next();
            }
        }
    }
    let result = if param_count == 0 {
        return_value.give_up(req);
        Owner::null()
    } else if param_count == 1 {
        // `ZVAL_COPY(return_value, tmp)` of the one element, then the
        // array destroyed.
        let first = walk::rows(req, *return_value)
            .next()
            .map_or_else(Value::null, |(_, v)| v.deref());
        let out = Owner::take(req, first);
        return_value.give_up(req);
        out
    } else {
        return_value.into_value()
    };
    if let (Some(out), Some(head)) = (soap_headers, head) {
        let read = (|| -> Res<()> {
            let mut headers = PhpArray::new(req)?;
            let mut trav = head.children();
            while let Some(t) = trav {
                if t.kind() == Kind::Element {
                    let mut enc = None;
                    if let Some(hdrs) = hdrs {
                        let mut key = Vec::new();
                        if let Some(ns) = t.ns() {
                            key.extend_from_slice(&crate::xml::href(ns));
                            key.push(b':');
                        }
                        key.extend_from_slice(&t.name());
                        if let Some(h) = hdrs.get_str(&key) {
                            enc = h.encode;
                        }
                    }
                    let val = match decode(req, cx, enc, t) {
                        Ok(val) => val,
                        Err(err) => {
                            headers.give_up(req);
                            return Err(err);
                        }
                    };
                    if let Err(thrown) = headers.set_symtable_str(req, &t.name(), val) {
                        headers.give_up(req);
                        return Err(thrown.into());
                    }
                }
                trav = t.next();
            }
            out.set(req, Untyped(headers.into_value()))?;
            Ok(())
        })();
        if let Err(err) = read {
            result.give_up(req);
            return Err(err);
        }
    }
    Ok(result)
}

//! `php_sdl.c`'s reading of a WSDL: `load_wsdl_ex` over a document and
//! the ones it imports, then `load_wsdl`'s walk of the services, ports,
//! bindings and operations into the description's functions, each
//! binding's SOAP 1.1, SOAP 1.2 or HTTP attributes, its bodies, headers,
//! header faults and faults (`wsdl_soap_binding_body`,
//! `wsdl_soap_binding_header`, `wsdl_message`).

use php_ext::raising::{Thrown, leave_pending};
use php_ext::request::{Request, StreamCtx};
use php_ext::values::{Owner, Value, ValueRef};
use php_ext_libxml::ffi::{Node, Raised};

use crate::classes::string;
use crate::model::{
    Binding, BindingType, Body, Fault, Function, Header, Param, RPC_SOAP12_NAMESPACE, RpcEncoding,
    SOAP_1_1_ENC_NAMESPACE, SOAP_1_2_ENC_NAMESPACE, Sdl, SoapBinding, SoapBindingFunction,
    SoapFaultBinding, Style, Use, WSDL_HTTP_TRANSPORT, WSDL_HTTP11_NAMESPACE,
    WSDL_HTTP12_NAMESPACE, WSDL_NAMESPACE, WSDL_SOAP11_NAMESPACE, WSDL_SOAP12_NAMESPACE,
    XSD_NAMESPACE,
};
use crate::schema::{
    get_encoder_from_prefix_mut, load_schema, lossy, schema_location_construct_uri, schema_pass2,
};
use crate::sdl::get_element;
use crate::table::{Key, Table};
use crate::xml::{
    attr_content, attribute, get_attribute, get_node, get_node_with_attribute, href, node_is_equal,
    parse_file_before_fatal,
};
use crate::{Error, Res, soap_error};

/// `SOAP_TRANSPORT_HTTP`.
const SOAP_TRANSPORT_HTTP: i32 = 1;

/// `sdlCtx`: the description being read, the documents read so far by
/// their URI (freed with it), the WSDL's messages, bindings,
/// `portTypes` and `services` by name, the schema's global attributes and
/// attribute groups until pass two, and the stream whose `http.header`
/// `sdl_set_uri_credentials` changed, with the header it had.
pub(crate) struct Ctx {
    pub(crate) sdl: Sdl,
    pub(crate) docs: Table<Node>,
    messages: Table<Node>,
    bindings: Table<Node>,
    port_types: Table<Node>,
    services: Table<Node>,
    pub(crate) attributes: Option<Table<crate::model::Attribute>>,
    pub(crate) attribute_groups: Option<Table<crate::model::TypeId>>,
    /// `fixupInProgress`: the types `schema_pass2` is fixing up, one
    /// inside the other, by which a group or an attribute group that
    /// names itself is refused.
    pub(crate) fixup_in_progress: std::collections::HashSet<crate::model::TypeId>,
    context: Option<StreamCtx>,
    old_header: Option<Owner<Value>>,
}

impl Ctx {
    fn new(source: &[u8]) -> Ctx {
        Ctx {
            sdl: Sdl {
                source: Some(source.to_vec()),
                ..Sdl::default()
            },
            docs: Table::new(),
            messages: Table::new(),
            bindings: Table::new(),
            port_types: Table::new(),
            services: Table::new(),
            attributes: None,
            attribute_groups: None,
            fixup_in_progress: std::collections::HashSet::new(),
            context: None,
            old_header: None,
        }
    }
}

impl Drop for Ctx {
    /// `delete_document` over `ctx.docs`, on every way out.
    fn drop(&mut self) {
        for doc in self.docs.values() {
            doc.free_document();
        }
    }
}

/// `soap_error0(E_ERROR, "Parsing WSDL: ...")`.
fn wsdl_error(text: &str) -> Error {
    soap_error(format_args!("Parsing WSDL: {text}"))
}

/// `Parsing WSDL: Unexpected WSDL element <%s>`, `SAFE_STR` of the name.
fn unexpected(node: Node) -> Error {
    wsdl_error(&format!(
        "Unexpected WSDL element <{}>",
        lossy(&node.name())
    ))
}

/// `is_wsdl_element`: whether a node is WSDL's own; an extension element
/// that says it is `wsdl:required` is an error.
fn is_wsdl_element(node: Node) -> Res<bool> {
    if let Some(ns) = node.ns() {
        let h = href(ns);
        if h != WSDL_NAMESPACE {
            if let Some(attr) = get_attribute(node.properties(), b"required", Some(WSDL_NAMESPACE))
            {
                let c = attr_content(attr);
                if c == b"1" || c == b"true" {
                    return Err(wsdl_error(&format!(
                        "Unknown required WSDL extension '{}'",
                        lossy(&h)
                    )));
                }
            }
            return Ok(false);
        }
    }
    Ok(true)
}

/// The length of a URI's scheme and authority: up to the first `/`
/// after `://`, none where it has no `://`.
fn authority_len(uri: &[u8]) -> Option<usize> {
    let s = uri.windows(3).position(|w| w == b"://")?;
    Some(
        uri[s + 3..]
            .iter()
            .position(|c| *c == b'/')
            .map_or(uri.len(), |p| s + 3 + p),
    )
}

/// The length an authority is compared at: `:80` dropped from an
/// `http` one and `:443` from an `https` one.
fn default_port_dropped(uri: &[u8], mut l: usize) -> usize {
    if l > 11 && uri[4] == b':' && uri[l - 3] == b':' && uri[l - 2] == b'8' && uri[l - 1] == b'0' {
        l -= 3;
    }
    if l > 13
        && uri[4] == b's'
        && uri[l - 4] == b':'
        && uri[l - 3] == b'4'
        && uri[l - 2] == b'4'
        && uri[l - 1] == b'3'
    {
        l -= 4;
    }
    l
}

/// `sdl_set_uri_credentials`: where `uri` is on another server than the
/// WSDL, the `Authorization: Basic` line taken out of the `http.header`
/// of the stream libxml loads with, until
/// [`sdl_restore_uri_credentials`].
fn sdl_set_uri_credentials(req: &mut Request, ctx: &mut Ctx, uri: &[u8]) -> Result<(), Thrown> {
    let source = ctx.sdl.source.clone().unwrap_or_default();
    let Some(mut l1) = authority_len(&source) else {
        return Ok(());
    };
    let Some(mut l2) = authority_len(uri) else {
        return Ok(());
    };
    if l1 != l2 {
        l1 = default_port_dropped(&source, l1);
        l2 = default_port_dropped(uri, l2);
    }
    if l1 == l2 && source[..l1] == uri[..l1] {
        return Ok(());
    }
    // `php_libxml_switch_context(NULL, &context)` and back: the stream
    // libxml loads with, read where it stands.
    let context = php_ext_libxml::switch_stream_ctx(req, None);
    let resource = context
        .as_deref()
        .and_then(php_ext::values::Value::as_resource);
    if let Some(none) = php_ext_libxml::switch_stream_ctx(req, context) {
        none.give_up(req);
    }
    let Some(resource) = resource else {
        return Ok(());
    };
    // `php_stream_context_from_zval(context_ptr, 1)`.
    let resource = StreamCtx::fetch(req, resource)?;
    ctx.context = Some(resource);
    let Some(header) = resource.option(req, b"http", b"header") else {
        return Ok(());
    };
    let ValueRef::Str(s) = header.as_ref() else {
        return Ok(());
    };
    let bytes = s.as_bytes().to_vec();
    let Some(at) = bytes
        .windows(b"Authorization: Basic".len())
        .position(|w| w == b"Authorization: Basic")
    else {
        return Ok(());
    };
    if at != 0 && bytes[at - 1] != b'\n' && bytes[at - 1] != b'\r' {
        return Ok(());
    }
    let Some(crlf) = bytes[at..].windows(2).position(|w| w == b"\r\n") else {
        return Ok(());
    };
    let rest = at + crlf + 2;
    let mut new_header = bytes[..at].to_vec();
    new_header.extend_from_slice(&bytes[rest..]);
    // `ZVAL_COPY(&ctx->old_header, header)`, kept once the new one is set.
    let old_header = Owner::take(req, header);
    let value = match string(req, &new_header) {
        Ok(value) => value,
        Err(thrown) => {
            old_header.give_up(req);
            return Err(thrown);
        }
    };
    let set = resource.set_option(req, b"http", b"header", &value);
    value.give_up(req);
    match set {
        Ok(()) => {
            ctx.old_header = Some(old_header);
            Ok(())
        }
        Err(thrown) => {
            old_header.give_up(req);
            Err(thrown)
        }
    }
}

/// `sdl_restore_uri_credentials`: the `http.header` put back.
fn sdl_restore_uri_credentials(req: &mut Request, ctx: &mut Ctx) -> Result<(), Thrown> {
    let context = ctx.context.take();
    if let Some(old) = ctx.old_header.take() {
        let set = match context {
            Some(c) => c.set_option(req, b"http", b"header", &old),
            None => Ok(()),
        };
        old.give_up(req);
        set?;
    }
    Ok(())
}

/// The document at `uri` parsed under the client's credentials, as
/// `load_wsdl_ex` and `load_schema` bracket `soap_xmlParseFile` with
/// `sdl_set_uri_credentials` and `sdl_restore_uri_credentials`: none
/// where it was not well formed, beside what a stream wrapper threw
/// meanwhile ([`parse_file_before_fatal`]). Where the restore fails, the
/// parse is given back first, the throwable left pending.
pub(crate) fn parse_with_credentials(
    req: &mut Request,
    ctx: &mut Ctx,
    uri: &[u8],
) -> Res<(Option<Node>, Option<Thrown>)> {
    sdl_set_uri_credentials(req, ctx, uri)?;
    let parsed = parse_file_before_fatal(req, uri);
    let restored = sdl_restore_uri_credentials(req, ctx);
    let (doc, pending) = parsed?;
    if let Err(thrown) = restored {
        if let Some(d) = doc {
            d.free_document();
        }
        if let Some(pending) = pending {
            leave_pending(req, pending);
        }
        return Err(Error::Thrown(thrown));
    }
    Ok((doc, pending))
}

/// The `name` attribute of a WSDL declaration, by which it is added to
/// one of `sdlCtx`'s tables.
fn add_named(table: &mut Table<Node>, node: Node, what: &str) -> Res<()> {
    match attribute(node, b"name").map(attr_content) {
        Some(name) if !name.is_empty() => {
            let shown = lossy(&name).into_owned();
            if !table.add(Key::Str(name), node) {
                return Err(wsdl_error(&format!("<{what}> '{shown}' already defined")));
            }
            Ok(())
        }
        _ => Err(wsdl_error(&format!("<{what}> has no name attribute"))),
    }
}

/// `load_wsdl_ex`: the document at `struri`, once, its types' schemas
/// loaded and its imports followed, its messages, `portTypes`, bindings
/// and `services` put in `sdlCtx` by name; an imported document that is
/// a bare schema loaded as one.
fn load_wsdl_ex(req: &mut Request, struri: &[u8], ctx: &mut Ctx, include: bool) -> Res<()> {
    if ctx.docs.contains(&Key::Str(struri.to_vec())) {
        return Ok(());
    }
    let (doc, pending) = parse_with_credentials(req, ctx, struri)?;
    let Some(wsdl) = doc else {
        let cause = Raised::last().map(|e| format!(" : {}", lossy(&e.message)));
        return Err(Error::soap_after(
            format_args!(
                "Parsing WSDL: Couldn't load from '{}'{}",
                lossy(struri),
                cause.unwrap_or_default()
            ),
            pending,
        ));
    };
    ctx.docs.add(Key::Str(struri.to_vec()), wsdl);

    let root = wsdl.children();
    let Some(definitions) = get_node(root, b"definitions", Some(WSDL_NAMESPACE)) else {
        if include && let Some(schema) = get_node(root, b"schema", Some(XSD_NAMESPACE)) {
            return load_schema(req, ctx, schema);
        }
        return Err(wsdl_error(&format!(
            "Couldn't find <definitions> in '{}'",
            lossy(struri)
        )));
    };
    if !include && let Some(tns) = attribute(definitions, b"targetNamespace") {
        ctx.sdl.target_ns = Some(attr_content(tns));
    }

    let mut trav = definitions.children();
    while let Some(t) = trav {
        trav = t.next();
        if !is_wsdl_element(t)? {
            continue;
        }
        if node_is_equal(t, Some(b"types"), None) {
            let mut trav2 = t.children();
            while let Some(t2) = trav2 {
                if node_is_equal(t2, Some(b"schema"), Some(XSD_NAMESPACE)) {
                    load_schema(req, ctx, t2)?;
                } else if is_wsdl_element(t2)? && !node_is_equal(t2, Some(b"documentation"), None) {
                    return Err(unexpected(t2));
                }
                trav2 = t2.next();
            }
        } else if node_is_equal(t, Some(b"import"), None) {
            if let Some(location) = attribute(t, b"location")
                && let Some(uri) = schema_location_construct_uri(location)
            {
                load_wsdl_ex(req, &uri, ctx, true)?;
            }
        } else if node_is_equal(t, Some(b"message"), None) {
            add_named(&mut ctx.messages, t, "message")?;
        } else if node_is_equal(t, Some(b"portType"), None) {
            add_named(&mut ctx.port_types, t, "portType")?;
        } else if node_is_equal(t, Some(b"binding"), None) {
            add_named(&mut ctx.bindings, t, "binding")?;
        } else if node_is_equal(t, Some(b"service"), None) {
            add_named(&mut ctx.services, t, "service")?;
        } else if !node_is_equal(t, Some(b"documentation"), None) {
            return Err(unexpected(t));
        }
    }
    Ok(())
}

/// The name after the last colon of a `QName`.
fn local_part(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|c| *c == b':') {
        Some(at) => &qname[at + 1..],
        None => qname,
    }
}

/// An `encodingStyle` attribute: SOAP 1.1's or 1.2's, else an error, and
/// one that is required where it is absent.
fn encoding_style(node: Node) -> Res<RpcEncoding> {
    let Some(attr) = attribute(node, b"encodingStyle") else {
        return Err(wsdl_error("Unspecified encodingStyle"));
    };
    let c = attr_content(attr);
    if c == SOAP_1_1_ENC_NAMESPACE {
        Ok(RpcEncoding::Soap11)
    } else if c == SOAP_1_2_ENC_NAMESPACE {
        Ok(RpcEncoding::Soap12)
    } else {
        Err(wsdl_error(&format!(
            "Unknown encodingStyle '{}'",
            lossy(&c)
        )))
    }
}

/// `ns:name`, or `name` where there is no namespace: a header's key.
fn header_key(h: &Header) -> Key {
    let mut key = Vec::new();
    if let Some(ns) = &h.ns {
        key.extend_from_slice(ns);
        key.push(b':');
    }
    key.extend_from_slice(&h.name);
    Key::Str(key)
}

/// `wsdl_soap_binding_header`: a `<soap:header>` (or, for `fault`, a
/// `<soap:headerfault>`) over its message's part, with its header faults.
fn wsdl_soap_binding_header(
    ctx: &mut Ctx,
    header: Node,
    wsdl_soap_namespace: &[u8],
    fault: bool,
) -> Res<Header> {
    let Some(tmp) = attribute(header, b"message") else {
        return Err(wsdl_error("Missing message attribute for <header>"));
    };
    let message_name = attr_content(tmp);
    let Some(message) = ctx.messages.get_str(local_part(&message_name)).copied() else {
        return Err(wsdl_error(&format!(
            "Missing <message> with name '{}'",
            lossy(&message_name)
        )));
    };
    let Some(tmp) = attribute(header, b"part") else {
        return Err(wsdl_error("Missing part attribute for <header>"));
    };
    let part_name = attr_content(tmp);
    let Some(part) = get_node_with_attribute(
        message.children(),
        Some(b"part"),
        Some(WSDL_NAMESPACE),
        b"name",
        &part_name,
        None,
    ) else {
        return Err(wsdl_error(&format!(
            "Missing part '{}' in <message>",
            lossy(&part_name)
        )));
    };
    let use_ = match attribute(header, b"use") {
        Some(u) if attr_content(u) == b"encoded" => Use::Encoded,
        _ => Use::Literal,
    };
    let mut h = Header {
        name: part_name,
        ns: attribute(header, b"namespace").map(attr_content),
        use_,
        encoding_style: RpcEncoding::Default,
        element: None,
        encode: None,
        headerfaults: None,
    };
    if h.use_ == Use::Encoded {
        h.encoding_style = encoding_style(header)?;
    }
    if let Some(t) = attribute(part, b"type") {
        h.encode = get_encoder_from_prefix_mut(&mut ctx.sdl, part, &attr_content(t));
    } else if let Some(e) = attribute(part, b"element") {
        h.element = get_element(&ctx.sdl, part, &attr_content(e));
        if let Some(el) = h.element.and_then(|id| ctx.sdl.ty(id)) {
            h.encode = el.encode;
            if h.ns.is_none() && el.namens.is_some() {
                h.ns.clone_from(&el.namens);
            }
            if let Some(name) = &el.name {
                h.name.clone_from(name);
            }
        }
    }
    if !fault {
        let mut trav = header.children();
        while let Some(t) = trav {
            if node_is_equal(t, Some(b"headerfault"), Some(wsdl_soap_namespace)) {
                let hf = wsdl_soap_binding_header(ctx, t, wsdl_soap_namespace, true)?;
                let key = header_key(&hf);
                h.headerfaults.get_or_insert_with(Table::new).add(key, hf);
            } else if is_wsdl_element(t)? && !node_is_equal(t, Some(b"documentation"), None) {
                return Err(unexpected(t));
            }
            trav = t.next();
        }
    }
    Ok(h)
}

/// `wsdl_soap_binding_body`: an operation's `<input>` or `<output>`
/// binding: its `<soap:body>`'s use, namespace, `parts` (which keeps
/// only the parameters named, in that order) and encoding style, and
/// its `<soap:header>`s.
fn wsdl_soap_binding_body(
    ctx: &mut Ctx,
    node: Node,
    wsdl_soap_namespace: &[u8],
    binding: &mut Body,
    params: &mut Table<Param>,
) -> Res<()> {
    let mut trav = node.children();
    while let Some(t) = trav {
        if node_is_equal(t, Some(b"body"), Some(wsdl_soap_namespace)) {
            binding.use_ = match attribute(t, b"use") {
                Some(u) if attr_content(u) == b"literal" => Use::Literal,
                _ => Use::Encoded,
            };
            if let Some(ns) = attribute(t, b"namespace") {
                binding.ns = Some(attr_content(ns));
            }
            if let Some(parts) = attribute(t, b"parts") {
                let parts = attr_content(parts);
                let mut ht = Table::new();
                for part in parts.split(|c| *c == b' ').filter(|p| !p.is_empty()) {
                    let found = params
                        .values_mut()
                        .find(|p| p.param_name.as_deref() == Some(part));
                    let Some(param) = found else {
                        return Err(wsdl_error(&format!(
                            "Missing part '{}' in <message>",
                            lossy(part)
                        )));
                    };
                    ht.push(param.clone());
                    param.param_name = None;
                }
                *params = ht;
            }
            if binding.use_ == Use::Encoded {
                binding.encoding_style = encoding_style(t)?;
            }
        } else if node_is_equal(t, Some(b"header"), Some(wsdl_soap_namespace)) {
            let h = wsdl_soap_binding_header(ctx, t, wsdl_soap_namespace, false)?;
            let key = header_key(&h);
            binding.headers.get_or_insert_with(Table::new).add(key, h);
        } else if is_wsdl_element(t)? && !node_is_equal(t, Some(b"documentation"), None) {
            return Err(unexpected(t));
        }
        trav = t.next();
    }
    Ok(())
}

/// `wsdl_message`: the parts of the message named, in order, each with
/// its encoder by `type` or its element by `element`.
fn wsdl_message(ctx: &mut Ctx, message_name: &[u8]) -> Res<Table<Param>> {
    let Some(message) = ctx.messages.get_str(local_part(message_name)).copied() else {
        return Err(wsdl_error(&format!(
            "Missing <message> with name '{}'",
            lossy(message_name)
        )));
    };
    let mut parameters = Table::new();
    let mut trav = message.children();
    while let Some(t) = trav {
        if let Some(ns) = t.ns()
            && href(ns) != WSDL_NAMESPACE
        {
            return Err(wsdl_error(&format!(
                "Unexpected extensibility element <{}>",
                lossy(&t.name())
            )));
        }
        trav = t.next();
        if node_is_equal(t, Some(b"documentation"), None) {
            continue;
        }
        if !node_is_equal(t, Some(b"part"), None) {
            return Err(unexpected(t));
        }
        let Some(name) = attribute(t, b"name") else {
            return Err(wsdl_error(&format!(
                "No name associated with <part> '{}'",
                lossy(&message.name())
            )));
        };
        let mut param = Param {
            order: 0,
            param_name: Some(attr_content(name)),
            ..Param::default()
        };
        if let Some(ty) = attribute(t, b"type") {
            param.encode = get_encoder_from_prefix_mut(&mut ctx.sdl, t, &attr_content(ty));
        } else if let Some(element) = attribute(t, b"element") {
            param.element = get_element(&ctx.sdl, t, &attr_content(element));
            param.encode = param
                .element
                .and_then(|id| ctx.sdl.ty(id))
                .and_then(|e| e.encode);
        }
        parameters.push(param);
    }
    Ok(parameters)
}

/// What a `<port>`'s address says of its binding.
struct Address {
    node: Node,
    soap_namespace: Option<&'static [u8]>,
    binding_type: BindingType,
}

/// The `<soap:address>` (1.1, 1.2 or SOAP 1.2 RPC) or `<http:address>`
/// of a `<port>`, the last one found; any other WSDL element an error.
fn port_address(port: Node) -> Res<Option<Address>> {
    let mut address: Option<Address> = None;
    let mut trav = port.children();
    while let Some(t) = trav {
        if node_is_equal(t, Some(b"address"), None)
            && let Some(ns) = t.ns()
        {
            let h = href(ns);
            let found = [
                (
                    WSDL_SOAP11_NAMESPACE,
                    Some(WSDL_SOAP11_NAMESPACE),
                    BindingType::Soap,
                ),
                (
                    WSDL_SOAP12_NAMESPACE,
                    Some(WSDL_SOAP12_NAMESPACE),
                    BindingType::Soap,
                ),
                (
                    RPC_SOAP12_NAMESPACE,
                    Some(RPC_SOAP12_NAMESPACE),
                    BindingType::Soap,
                ),
                (WSDL_HTTP11_NAMESPACE, None, BindingType::Http),
                (WSDL_HTTP12_NAMESPACE, None, BindingType::Http),
            ]
            .into_iter()
            .find(|(n, _, _)| *n == h.as_slice());
            if let Some((_, soap_namespace, binding_type)) = found {
                address = Some(Address {
                    node: t,
                    soap_namespace: soap_namespace
                        .or(address.as_ref().and_then(|a| a.soap_namespace)),
                    binding_type,
                });
            }
        }
        let is_address = address.as_ref().is_some_and(|a| a.node == t);
        if !is_address && is_wsdl_element(t)? && !node_is_equal(t, Some(b"documentation"), None) {
            return Err(unexpected(t));
        }
        trav = t.next();
    }
    Ok(address)
}

/// The `<soap:binding>` of a binding: its style (document by default)
/// and its `transport`; `None` for one other than HTTP, which
/// sends the `<port>` on to the next.
fn soap_binding(binding: Node, wsdl_soap_namespace: &[u8]) -> Option<SoapBinding> {
    let mut soap = SoapBinding {
        style: Style::Document,
        transport: 0,
    };
    if let Some(node) = get_node(binding.children(), b"binding", Some(wsdl_soap_namespace)) {
        if attribute(node, b"style").is_some_and(|s| attr_content(s) == b"rpc") {
            soap.style = Style::Rpc;
        }
        if let Some(transport) = attribute(node, b"transport") {
            if attr_content(transport) == WSDL_HTTP_TRANSPORT {
                soap.transport = SOAP_TRANSPORT_HTTP;
            } else {
                return None;
            }
        }
    }
    Some(soap)
}

/// The `<fault>`s of an operation's `<portType>`, each over its one-part
/// message, with the `<soap:fault>` of the binding's `<fault>` of that
/// name.
fn operation_faults(
    ctx: &mut Ctx,
    port_type_operation: Node,
    operation: Node,
    op_name: &[u8],
    soap_namespace: Option<&[u8]>,
) -> Res<Option<Table<Fault>>> {
    let mut faults: Option<Table<Fault>> = None;
    let mut trav = port_type_operation.children();
    while let Some(f) = trav {
        trav = f.next();
        if !node_is_equal(f, Some(b"fault"), Some(WSDL_NAMESPACE)) {
            continue;
        }
        let Some(name) = attribute(f, b"name") else {
            return Err(wsdl_error(&format!(
                "Missing name for <fault> of '{}'",
                lossy(op_name)
            )));
        };
        let Some(message) = attribute(f, b"message") else {
            return Err(wsdl_error(&format!(
                "Missing name for <output> of '{}'",
                lossy(op_name)
            )));
        };
        let message = attr_content(message);
        let mut fault = Fault {
            name: attr_content(name),
            details: Some(wsdl_message(ctx, &message)?),
            binding: None,
        };
        if fault.details.as_ref().is_some_and(|d| d.len() > 1) {
            return Err(wsdl_error(&format!(
                "The fault message '{}' must have a single part",
                lossy(&message)
            )));
        }
        if let Some(ns) = soap_namespace
            && let Some(soap_fault) = get_node_with_attribute(
                operation.children(),
                Some(b"fault"),
                Some(WSDL_NAMESPACE),
                b"name",
                &fault.name,
                None,
            )
        {
            let mut nodes = soap_fault.children();
            while let Some(n) = nodes {
                if node_is_equal(n, Some(b"fault"), Some(ns)) {
                    let use_ = match attribute(n, b"use") {
                        Some(u) if attr_content(u) == b"encoded" => Use::Encoded,
                        _ => Use::Literal,
                    };
                    let mut binding = SoapFaultBinding {
                        ns: attribute(n, b"namespace").map(attr_content),
                        use_,
                        encoding_style: RpcEncoding::Default,
                    };
                    if use_ == Use::Encoded {
                        binding.encoding_style = encoding_style(n)?;
                    }
                    fault.binding = Some(binding);
                } else if is_wsdl_element(n)? && !node_is_equal(n, Some(b"documentation"), None) {
                    return Err(unexpected(n));
                }
                nodes = n.next();
            }
        }
        let key = fault.name.clone();
        if !faults
            .get_or_insert_with(Table::new)
            .add(Key::Str(key.clone()), fault)
        {
            return Err(wsdl_error(&format!(
                "<fault> with name '{}' already defined in '{}'",
                lossy(&key),
                lossy(op_name)
            )));
        }
    }
    Ok(faults)
}

/// One `<operation>` of a binding made a function: its SOAP attributes,
/// its request and response parameters with their bodies, and its
/// faults.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
fn binding_operation(
    ctx: &mut Ctx,
    operation: Node,
    port_type: Node,
    tmpbinding: &Binding,
    soap_namespace: Option<&[u8]>,
) -> Res<Function> {
    let Some(op_name) = attribute(operation, b"name") else {
        return Err(wsdl_error("Missing 'name' attribute for <operation>"));
    };
    let op_name = attr_content(op_name);
    let mut trav3 = operation.children();
    while let Some(t3) = trav3 {
        let soap_op =
            soap_namespace.is_some_and(|ns| node_is_equal(t3, Some(b"operation"), Some(ns)));
        if !soap_op
            && is_wsdl_element(t3)?
            && !node_is_equal(t3, Some(b"input"), None)
            && !node_is_equal(t3, Some(b"output"), None)
            && !node_is_equal(t3, Some(b"fault"), None)
            && !node_is_equal(t3, Some(b"documentation"), None)
        {
            return Err(unexpected(t3));
        }
        trav3 = t3.next();
    }
    let Some(port_type_operation) = get_node_with_attribute(
        port_type.children(),
        Some(b"operation"),
        Some(WSDL_NAMESPACE),
        b"name",
        &op_name,
        None,
    ) else {
        return Err(wsdl_error(&format!(
            "Missing <portType>/<operation> with name '{}'",
            lossy(&op_name)
        )));
    };
    let mut function = Function {
        function_name: op_name.clone(),
        ..Function::default()
    };
    if let Some(ns) = soap_namespace {
        let style = tmpbinding.attributes.map_or(Style::Document, |a| a.style);
        let mut fb = SoapBindingFunction {
            soap_action: None,
            style,
            input: Body::default(),
            output: Body::default(),
        };
        if let Some(soap_operation) = get_node(operation.children(), b"operation", Some(ns)) {
            if let Some(action) = attribute(soap_operation, b"soapAction") {
                fb.soap_action = Some(attr_content(action));
            }
            fb.style = match attribute(soap_operation, b"style") {
                Some(s) if attr_content(s) == b"rpc" => Style::Rpc,
                Some(_) => Style::Document,
                None => style,
            };
        }
        function.binding_attributes = Some(fb);
    }

    if let Some(input) = get_node(
        port_type_operation.children(),
        b"input",
        Some(WSDL_NAMESPACE),
    ) {
        let Some(message) = attribute(input, b"message") else {
            return Err(wsdl_error(&format!(
                "Missing name for <input> of '{}'",
                lossy(&op_name)
            )));
        };
        let mut params = wsdl_message(ctx, &attr_content(message))?;
        function.request_name = Some(op_name.clone());
        if let (Some(ns), Some(fb)) = (soap_namespace, function.binding_attributes.as_mut())
            && let Some(input) = get_node(operation.children(), b"input", Some(WSDL_NAMESPACE))
        {
            wsdl_soap_binding_body(ctx, input, ns, &mut fb.input, &mut params)?;
        }
        function.request_parameters = Some(params);
    }

    if let Some(output) = get_node(
        port_type_operation.children(),
        b"output",
        Some(WSDL_NAMESPACE),
    ) {
        let Some(message) = attribute(output, b"message") else {
            return Err(wsdl_error(&format!(
                "Missing name for <output> of '{}'",
                lossy(&op_name)
            )));
        };
        let mut params = wsdl_message(ctx, &attr_content(message))?;
        let mut response = op_name.clone();
        response.extend_from_slice(b"Response");
        function.response_name = Some(response);
        if let (Some(ns), Some(fb)) = (soap_namespace, function.binding_attributes.as_mut())
            && let Some(output) = get_node(operation.children(), b"output", Some(WSDL_NAMESPACE))
        {
            wsdl_soap_binding_body(ctx, output, ns, &mut fb.output, &mut params)?;
        }
        function.response_parameters = Some(params);
    }

    function.faults = operation_faults(
        ctx,
        port_type_operation,
        operation,
        &op_name,
        soap_namespace,
    )?;
    Ok(function)
}

/// The function added by its lowercased name, or pushed where a function
/// of that name is there; a request name other than the function's
/// added to `requests`.
fn add_function(sdl: &mut Sdl, function: Function) {
    let lc = function.function_name.to_ascii_lowercase();
    let request = function
        .request_name
        .clone()
        .filter(|r| *r != function.function_name);
    let at = sdl.functions.len();
    if !sdl.functions.add(Key::Str(lc), function.clone()) {
        sdl.functions.push(function);
    }
    if let Some(r) = request {
        sdl.requests
            .get_or_insert_with(Table::new)
            .add(Key::Str(r.to_ascii_lowercase()), at);
    }
}

/// `load_wsdl`: the document and what it imports read, the schemas'
/// pass two made, then every SOAP `<port>` of every `<service>` (an HTTP
/// one only where it is the last `<port>` there is) made a binding with its
/// operations as functions.
#[allow(
    clippy::too_many_lines,
    reason = "php's function, one Rust function per C function"
)]
pub(crate) fn load_wsdl(req: &mut Request, struri: &[u8]) -> Res<Sdl> {
    let mut ctx = Ctx::new(struri);
    load_wsdl_ex(req, struri, &mut ctx, false)?;
    schema_pass2(&mut ctx)?;

    let n = ctx.services.len();
    if n == 0 {
        return Err(wsdl_error("Couldn't bind to service"));
    }
    let services: Vec<Node> = ctx.services.values().copied().collect();
    for (i, service) in services.into_iter().enumerate() {
        let mut has_soap_port = false;
        let mut trav = service.children();
        while let Some(port) = trav {
            trav = port.next();
            if !is_wsdl_element(port)? || node_is_equal(port, Some(b"documentation"), None) {
                continue;
            }
            if !node_is_equal(port, Some(b"port"), None) {
                return Err(unexpected(port));
            }
            let Some(binding_attr) = attribute(port, b"binding") else {
                return Err(wsdl_error("No binding associated with <port>"));
            };
            let address = port_address(port)?;
            let address = match address {
                Some(a) if a.binding_type == BindingType::Soap => a,
                found => {
                    if has_soap_port || port.next().is_some() || i + 1 < n {
                        continue;
                    }
                    match found {
                        Some(a) => a,
                        None => return Err(wsdl_error("No address associated with <port>")),
                    }
                }
            };
            has_soap_port = true;
            let Some(location) = attribute(address.node, b"location") else {
                return Err(wsdl_error("No location associated with <port>"));
            };
            let binding_name = attr_content(binding_attr);
            let ctype = local_part(&binding_name);
            let Some(binding) = ctx.bindings.get_str(ctype).copied() else {
                return Err(wsdl_error(&format!(
                    "No <binding> element with name '{}'",
                    lossy(ctype)
                )));
            };
            let soap_namespace = match address.binding_type {
                BindingType::Soap => address.soap_namespace,
                BindingType::Http => None,
            };
            let mut attributes = None;
            if let Some(ns) = soap_namespace {
                match soap_binding(binding, ns) {
                    Some(b) => attributes = Some(b),
                    None => continue,
                }
            }
            let Some(name) = attribute(binding, b"name") else {
                return Err(wsdl_error("Missing 'name' attribute for <binding>"));
            };
            let tmpbinding = Binding {
                name: attr_content(name),
                location: Some(attr_content(location)),
                binding_type: address.binding_type,
                attributes,
            };
            let Some(type_attr) = attribute(binding, b"type") else {
                return Err(wsdl_error("Missing 'type' attribute for <binding>"));
            };
            let type_value = attr_content(type_attr);
            let ptype = match type_value.iter().position(|c| *c == b':') {
                Some(at) => &type_value[at + 1..],
                None => &type_value[..],
            };
            let Some(port_type) = ctx.port_types.get_str(ptype).copied() else {
                return Err(wsdl_error(&format!(
                    "Missing <portType> with name '{}'",
                    lossy(&tmpbinding.name)
                )));
            };
            let binding_at = ctx.sdl.bindings.as_ref().map_or(0, Table::len);
            let mut trav2 = binding.children();
            while let Some(op) = trav2 {
                trav2 = op.next();
                let soap_binding_node =
                    soap_namespace.is_some_and(|ns| node_is_equal(op, Some(b"binding"), Some(ns)));
                if soap_binding_node
                    || !is_wsdl_element(op)?
                    || node_is_equal(op, Some(b"documentation"), None)
                {
                    continue;
                }
                if !node_is_equal(op, Some(b"operation"), None) {
                    return Err(unexpected(op));
                }
                let mut function =
                    binding_operation(&mut ctx, op, port_type, &tmpbinding, soap_namespace)?;
                function.binding = Some(binding_at);
                add_function(&mut ctx.sdl, function);
            }
            let bindings = ctx.sdl.bindings.get_or_insert_with(Table::new);
            let key = Key::Str(tmpbinding.name.clone());
            if !bindings.add(key, tmpbinding.clone()) {
                bindings.push(tmpbinding);
            }
        }
    }
    if ctx.sdl.bindings.as_ref().is_none_or(Table::is_empty) {
        return Err(wsdl_error(
            "Could not find any usable binding services in WSDL.",
        ));
    }
    Ok(std::mem::take(&mut ctx.sdl))
}

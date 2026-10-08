//! `php_xml.c`: parsing a SOAP message or a WSDL through `ext/libxml`'s
//! `Parser::soap`, and the node and attribute lookups every other file
//! walks a tree with.

use php_ext::raising::{Level, Thrown, uncaught_at};
use php_ext::request::Request;
use php_ext_libxml::Errors;
use php_ext_libxml::ffi::{Kind, Node, Ns, Parser, Source};

/// `is_blank`: only spaces, tabs, newlines and carriage returns.
fn is_blank(s: &[u8]) -> bool {
    s.iter().all(|c| matches!(c, b' ' | 0x9 | 0xa | 0xd))
}

/// `cleanup_xml_node`: blank text, comments and every other node that is
/// neither an element nor a CDATA section removed, recursively.
pub(crate) fn cleanup_xml_node(node: Node) {
    let mut trav = node.children();
    let mut del: Option<Node> = None;
    while let Some(t) = trav {
        if let Some(d) = del.take() {
            d.unlink();
            d.free();
        }
        match t.kind() {
            Kind::Text => {
                if is_blank(&t.content().unwrap_or_default()) {
                    del = Some(t);
                }
            }
            Kind::Element | Kind::CData => {
                if t.children().is_some() {
                    cleanup_xml_node(t);
                }
            }
            _ => del = Some(t),
        }
        trav = t.next();
    }
    if let Some(d) = del {
        d.unlink();
        d.free();
    }
}

/// `soap_xmlParseFile`: the document at `filename`, opened through the
/// request's streams, cleaned of insignificant nodes; none where it was
/// not well formed. What a program's stream code threw is the `Err`.
/// What libxml raised meanwhile reaches php's handler as
/// `php_libxml_ctx_error` raises it, through soap's, which drops a
/// throwing client's while it loads its WSDL: the `I/O warning` of a
/// WSDL that is not there is shown where the client does not throw.
///
/// ```php
/// try { new SoapClient('/nonexistent.wsdl', ['cache_wsdl' => WSDL_CACHE_NONE]); }
/// catch (SoapFault $e) { echo $e->getMessage(), "\n"; }
/// var_dump(error_get_last());
/// new SoapClient('/nonexistent.wsdl', ['exceptions' => false, 'cache_wsdl' => WSDL_CACHE_NONE]);
/// ```
/// ```expect exit=255
/// SOAP-ERROR: Parsing WSDL: Couldn't load from '/nonexistent.wsdl' : failed to load external entity "/nonexistent.wsdl"
///
/// NULL
///
/// Warning: SoapClient::__construct(): I/O warning : failed to load external entity "/nonexistent.wsdl" in %s on line 5
///
/// Fatal error: SOAP-ERROR: Parsing WSDL: Couldn't load from '/nonexistent.wsdl' : failed to load external entity "/nonexistent.wsdl"
///  in %s on line 5
/// ```
pub(crate) fn parse_file(req: &mut Request, filename: &[u8]) -> Result<Option<Node>, Thrown> {
    let (doc, raised, thrown) = Parser::soap(req, Source::File(filename));
    if let Some(t) = thrown {
        if let Some(d) = doc {
            d.free_document();
        }
        return Err(t);
    }
    Errors::route(req, raised);
    if let Some(d) = doc {
        cleanup_xml_node(d);
    }
    Ok(doc)
}

/// [`parse_file`] where the caller's `NULL` is an `E_ERROR`, a WSDL's or
/// an imported schema's: what a stream wrapper threw stays pending in
/// php, and `zend_error` renders it "Uncaught ..." at `E_WARNING` and
/// gives it up before the fatal (`Z/Zend/zend.c:1518`), so the parse
/// answers none. Under a client that throws, the warning is one the soap
/// handler drops while a WSDL is parsed, and the fault is all there is.
///
/// ```php
/// class W { public $context; function stream_open($p, $m, $o, &$x) { throw new RuntimeException("open $p"); } function url_stat($p, $f) { return []; } }
/// stream_wrapper_register('boom', 'W');
/// $i = '<xsd:schema xmlns:xsd="http://www.w3.org/2001/XMLSchema" targetNamespace="urn:t"><xsd:import namespace="urn:u" schemaLocation="boom://s.xsd"/></xsd:schema>';
/// foreach (["<types>$i</types>", '<import namespace="urn:v" location="boom://w.wsdl"/>'] as $body) {
///     try { new SoapClient('data://text/plain,' . urlencode("<definitions xmlns='http://schemas.xmlsoap.org/wsdl/' targetNamespace='urn:t'>$body</definitions>"), ['cache_wsdl' => WSDL_CACHE_NONE]); }
///     catch (SoapFault $e) { var_dump($e->getMessage(), $e->getPrevious()); }
/// }
/// ```
/// ```expect
/// string(67) "SOAP-ERROR: Parsing Schema: can't import schema from 'boom://s.xsd'"
/// NULL
/// string(110) "SOAP-ERROR: Parsing WSDL: Couldn't load from 'boom://w.wsdl' : failed to load external entity "boom://w.wsdl"
/// "
/// NULL
/// ```
pub(crate) fn parse_file_before_fatal(
    req: &mut Request,
    filename: &[u8],
) -> Result<Option<Node>, Thrown> {
    match parse_file(req, filename) {
        Err(t) => uncaught_at(req, t, Level::Warning).map(|()| None),
        r => r,
    }
}

/// `soap_xmlParseMemory`: the document in `buf`, not cleaned.
pub(crate) fn parse_memory(req: &mut Request, buf: &[u8]) -> Result<Option<Node>, Thrown> {
    let (doc, _raised, thrown) = Parser::soap(req, Source::Memory(buf));
    if let Some(t) = thrown {
        if let Some(d) = doc {
            d.free_document();
        }
        return Err(t);
    }
    Ok(doc)
}

/// `node_find_ns`: the node's namespace, or the default one in scope.
pub(crate) fn node_find_ns(node: Node) -> Option<Ns> {
    node.ns().or_else(|| node.search_ns(None))
}

/// The href of a namespace, empty where it has none.
pub(crate) fn href(ns: Ns) -> Vec<u8> {
    ns.href().unwrap_or_default()
}

/// `attr_is_equal_ex`: the attribute named `name`, in namespace `ns`, or
/// in none where `ns` is none.
pub(crate) fn attr_is_equal(attr: Node, name: &[u8], ns: Option<&[u8]>) -> bool {
    if !attr.has_name() || attr.name() != name {
        return false;
    }
    match (ns, attr.ns()) {
        (Some(want), Some(have)) => href(have) == want,
        (Some(_), None) | (None, Some(_)) => false,
        (None, None) => true,
    }
}

/// `node_is_equal_ex`: the node named `name` (any where none), and in
/// namespace `ns` where one is given.
pub(crate) fn node_is_equal(node: Node, name: Option<&[u8]>, ns: Option<&[u8]>) -> bool {
    let named = match name {
        None => true,
        Some(name) => node.has_name() && node.name() == name,
    };
    if !named {
        return false;
    }
    match ns {
        Some(want) => node_find_ns(node).is_some_and(|have| href(have) == want),
        None => true,
    }
}

/// `node_is_equal_ex_one_of`.
pub(crate) fn node_is_equal_one_of(node: Node, name: &[u8], namespaces: &[&[u8]]) -> bool {
    if !node.has_name() || node.name() != name {
        return false;
    }
    node_find_ns(node).is_some_and(|have| {
        let h = href(have);
        namespaces.contains(&h.as_slice())
    })
}

/// `get_attribute_any_ns`: from `attr` on, the first attribute named
/// `name` in whatever namespace.
pub(crate) fn get_attribute_any_ns(mut attr: Option<Node>, name: &[u8]) -> Option<Node> {
    while let Some(a) = attr {
        if a.has_name() && a.name() == name {
            return Some(a);
        }
        attr = a.next();
    }
    None
}

/// `get_attribute_ex`: from `attr` on, the first attribute
/// [`attr_is_equal`] finds.
pub(crate) fn get_attribute(
    mut attr: Option<Node>,
    name: &[u8],
    ns: Option<&[u8]>,
) -> Option<Node> {
    while let Some(a) = attr {
        if attr_is_equal(a, name, ns) {
            return Some(a);
        }
        attr = a.next();
    }
    None
}

/// The attribute of `node` named `name` in no namespace, `get_attribute`.
pub(crate) fn attribute(node: Node, name: &[u8]) -> Option<Node> {
    get_attribute(node.properties(), name, None)
}

/// `attr->children->content`: an attribute's text, empty where it has
/// none.
pub(crate) fn attr_content(attr: Node) -> Vec<u8> {
    attr.children().and_then(Node::content).unwrap_or_default()
}

/// `get_node_ex`: from `node` on, the first sibling [`node_is_equal`]
/// finds.
pub(crate) fn get_node(mut node: Option<Node>, name: &[u8], ns: Option<&[u8]>) -> Option<Node> {
    while let Some(n) = node {
        if node_is_equal(n, Some(name), ns) {
            return Some(n);
        }
        node = n.next();
    }
    None
}

/// `get_node_with_attribute_ex`.
pub(crate) fn get_node_with_attribute(
    mut node: Option<Node>,
    name: Option<&[u8]>,
    name_ns: Option<&[u8]>,
    attribute: &[u8],
    value: &[u8],
    attr_ns: Option<&[u8]>,
) -> Option<Node> {
    while let Some(mut n) = node {
        if let Some(name) = name {
            n = get_node(Some(n), name, name_ns)?;
        }
        if let Some(attr) = get_attribute(n.properties(), attribute, attr_ns)
            && attr_content(attr) == value
        {
            return Some(n);
        }
        node = n.next();
    }
    None
}

/// `parse_namespace`: `prefix:value` split at its last colon, a leading
/// colon leaving the whole as the value.
pub(crate) fn parse_namespace(inval: &[u8]) -> (&[u8], Option<&[u8]>) {
    match inval.iter().rposition(|c| *c == b':') {
        Some(at) if at != 0 => (&inval[at + 1..], Some(&inval[..at])),
        _ => (inval, None),
    }
}

/// The element children of `node` and every other kind, in order.
pub(crate) fn children(node: Node) -> impl Iterator<Item = Node> {
    std::iter::successors(node.children(), |n| n.next())
}

/// `xmlEncodeSpecialChars(doc, s)`: `<`, `>`, `&`, `"` and `\r` as
/// references.
pub(crate) fn encode_special_chars(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        match c {
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            b'"' => out.extend_from_slice(b"&quot;"),
            b'\r' => out.extend_from_slice(b"&#13;"),
            _ => out.push(c),
        }
    }
    out
}

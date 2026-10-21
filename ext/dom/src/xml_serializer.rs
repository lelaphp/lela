//! `xml_serializer.c`: the XML serialisation of the DOM Parsing
//! specification (<https://w3c.github.io/DOM-Parsing/>, 2021-05-02) as php
//! writes it for the `Dom\` classes, with the specification issues php
//! fixes fixed here too (28, 29, 38, 43, 44, 45, 47, 50, 52, 59 and 71),
//! and `xml_document.c`'s document handlers over it:
//! `php_new_dom_dump_node_to_str` and `php_new_dom_dump_node_to_file`.
//!
//! What dom writes goes through libxml's `xmlOutputBuffer` and what libxml
//! writes itself (a document's declaration, a DTD) through a save
//! `xmlSaveCtxt`, both converting into the document's encoding ([`Dump`]).
//! php recurses over the tree and raises "Maximum call stack size
//! reached" where the stack comes down to its limit; so does this, the
//! limit being the one `Limits::stack_overflowed` reads.

use std::ffi::c_int;

use php_ext::raising::Thrown;
use php_ext::request::{OwnedStream as _, Request, Streams as _};
use php_ext::values::PhpObject;
use php_ext_libxml::ffi::{self as f, Dump, Kind, Name, Node, Ns};

use crate::document::{XML_NS, XMLNS_NS};
use crate::private::{HTML_NS, Private, ns_is};

/// A step of the serialisation that failed: a write libxml refused, or
/// markup that well-formedness, where it is required, forbids; or the
/// stack come down to its limit on the way down the tree, php's `Error`.
#[derive(Clone, Copy)]
enum Failed {
    Refused,
    Deep,
}

type Step<T = ()> = Result<T, Failed>;

/// Two strings equal with null read as the empty string,
/// `dom_xml_str_equals_treat_nulls_as_empty`.
fn equal_or_empty(a: Option<&[u8]>, b: Option<&[u8]>) -> bool {
    a.unwrap_or_default() == b.unwrap_or_default()
}

/// The namespace prefix map (`dom_xml_ns_prefix_map`): a namespace, the
/// empty string for none, and its candidate prefixes in the order added.
#[derive(Clone, Default)]
struct PrefixMap(Vec<(Vec<u8>, Vec<Vec<u8>>)>);

impl PrefixMap {
    /// `dom_xml_ns_prefix_map_add`.
    fn add(&mut self, prefix: &[u8], ns: Option<&[u8]>) {
        let ns = ns.unwrap_or_default();
        match self.0.iter_mut().find(|(n, _)| n == ns) {
            Some((_, list)) => list.push(prefix.to_vec()),
            None => self.0.push((ns.to_vec(), vec![prefix.to_vec()])),
        }
    }

    /// `dom_get_candidates_list`.
    fn candidates(&self, ns: &[u8]) -> Option<&[Vec<u8>]> {
        self.0
            .iter()
            .find(|(n, _)| n == ns)
            .map(|(_, list)| list.as_slice())
    }

    /// `dom_prefix_found_in_ns_prefix_map`.
    fn found(&self, prefix: &[u8], ns: &[u8]) -> bool {
        self.candidates(ns)
            .is_some_and(|list| list.iter().any(|p| p == prefix))
    }
}

/// The local prefixes map (`dom_xml_local_prefix_map`): a prefix and the
/// namespace declared for it, the first one added kept.
#[derive(Default)]
struct LocalPrefixes(Vec<(Vec<u8>, Option<Vec<u8>>)>);

impl LocalPrefixes {
    /// `dom_xml_local_prefix_map_add`, which adds nothing for a prefix
    /// already present.
    fn add(&mut self, prefix: &[u8], ns: Option<&[u8]>) {
        if !self.0.iter().any(|(p, _)| p == prefix) {
            self.0.push((prefix.to_vec(), ns.map(<[u8]>::to_vec)));
        }
    }

    /// `dom_xml_local_prefix_map_find`.
    fn find(&self, prefix: &[u8]) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(p, _)| p == prefix)
            .and_then(|(_, ns)| ns.as_deref())
    }

    /// `dom_xml_local_prefix_map_conflicts`.
    fn conflicts(&self, prefix: &[u8], ns: &[u8]) -> bool {
        self.find(prefix)
            .is_some_and(|found| !equal_or_empty(Some(found), Some(ns)))
    }

    /// `dom_xml_local_prefix_map_contains`.
    fn contains(&self, prefix: &[u8]) -> bool {
        self.find(prefix).is_some()
    }
}

/// `dom_retrieve_a_preferred_prefix_string`, walking the candidates from
/// the last so the nearest declaration wins (issue 45).
fn preferred_prefix(
    map: &PrefixMap,
    local: &LocalPrefixes,
    preferred: Option<&[u8]>,
    ns: Option<&[u8]>,
) -> Option<Vec<u8>> {
    let ns = ns.unwrap_or_default();
    let list = map.candidates(ns)?;
    let mut last_non_conflicting = None;
    for prefix in list.iter().rev() {
        if !local.conflicts(prefix, ns) {
            if equal_or_empty(preferred, Some(prefix)) {
                return Some(prefix.clone());
            }
            if last_non_conflicting.is_none() {
                last_non_conflicting = Some(prefix);
            }
        }
    }
    last_non_conflicting.cloned()
}

/// `dom_xml_generate_a_prefix`: `ns1`, `ns2`, ..., skipping a prefix the
/// element declares (issue 44), added to both maps.
fn generate_prefix(
    map: &mut PrefixMap,
    local: &mut LocalPrefixes,
    ns: Option<&[u8]>,
    index: &mut u32,
) -> Vec<u8> {
    let prefix = loop {
        let candidate = format!("ns{index}").into_bytes();
        *index = index.wrapping_add(1);
        if !local.contains(&candidate) {
            break candidate;
        }
    };
    map.add(&prefix, ns);
    local.add(&prefix, ns);
    prefix
}

/// `dom_get_attribute_value`: the first child's content, the empty string
/// for none.
fn attribute_value(attr: Node) -> Vec<u8> {
    attr.children().and_then(Node::content).unwrap_or_default()
}

/// `dom_recording_the_namespace_information`: the element's `xmlns`
/// attributes recorded in both maps, and the value of its default
/// namespace declaration.
fn record_namespaces(
    map: &mut PrefixMap,
    local: &mut LocalPrefixes,
    element: Node,
) -> Option<Vec<u8>> {
    let mut default_namespace = None;
    let mut at = element.properties();
    while let Some(attr) = at {
        at = attr.next();
        if !ns_is(attr, XMLNS_NS) {
            continue;
        }
        if attr.ns().and_then(Ns::prefix).is_none() {
            default_namespace = Some(attribute_value(attr));
            continue;
        }
        let prefix = attr.name();
        let definition = attribute_value(attr);
        if definition == XML_NS || map.found(&prefix, &definition) {
            continue;
        }
        map.add(&prefix, Some(&definition));
        local.add(&prefix, Some(&definition));
    }
    default_namespace
}

/// `xmlIsCharQ`: the XML `Char` production.
fn is_xml_char(c: char) -> bool {
    matches!(u32::from(c), 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF)
}

/// `dom_xml_check_char_production`: valid UTF-8 of XML characters only.
fn chars_ok(content: &[u8]) -> Step {
    match std::str::from_utf8(content) {
        Ok(s) if s.chars().all(is_xml_char) => Ok(()),
        _ => Err(Failed::Refused),
    }
}

/// `dom_xml_should_format_element`: children, none of them text, an
/// entity reference or a CDATA section.
fn should_format(first: Node) -> bool {
    let mut at = Some(first);
    while let Some(child) = at {
        if matches!(child.kind(), Kind::Text | Kind::EntityRef | Kind::CData) {
            return false;
        }
        at = child.next();
    }
    true
}

/// The HTML elements step 14 writes as `<x />`.
const XML_VOID_ELEMENTS: [&[u8]; 19] = [
    b"area",
    b"base",
    b"basefont",
    b"bgsound",
    b"br",
    b"col",
    b"embed",
    b"frame",
    b"hr",
    b"img",
    b"input",
    b"keygen",
    b"link",
    b"menuitem",
    b"meta",
    b"param",
    b"source",
    b"track",
    b"wbr",
];

/// What an element's start tag declares: the prefix map its children
/// are given, its local prefixes and its default namespace declaration.
struct Scope {
    map: PrefixMap,
    local: LocalPrefixes,
    local_default: Option<Vec<u8>>,
}

/// `dom_qname_pair`.
struct QName {
    prefix: Option<Vec<u8>>,
    name: Vec<u8>,
}

/// An element, fragment or document whose children are being written:
/// what php keeps on the C stack across the recursive call.
struct Frame {
    /// The map the children are given.
    map: PrefixMap,
    /// The next child to write.
    next: Option<Node>,
    /// The namespace the children inherit.
    inherited: Option<Vec<u8>>,
    /// The children's indentation, negative for none.
    indent: i32,
    /// Whether each child and the end tag go on a line of their own.
    format: bool,
    /// The end tag an element closes with.
    end: Option<QName>,
}

/// One pass of `dom_xml_serialize`.
struct Serializer<'a> {
    req: &'a Request,
    dump: &'a mut Dump,
    private: Option<&'a Private>,
    well_formed: bool,
    /// `xmlSaveNoEmptyTags`.
    no_empty: bool,
    /// The generated namespace prefix index.
    index: u32,
}

impl Serializer<'_> {
    fn write(&mut self, bytes: &[u8]) -> Step {
        if self.dump.write(bytes) {
            Ok(())
        } else {
            Err(Failed::Refused)
        }
    }

    /// `dom_xml_output_qname`.
    fn qname(&mut self, qname: &QName) -> Step {
        if let Some(prefix) = &qname.prefix {
            self.write(prefix)?;
            self.write(b":")?;
        }
        self.write(&qname.name)
    }

    /// `dom_xml_output_indents`.
    fn indents(&mut self, indent: i32) -> Step {
        self.write(b"\n")?;
        for _ in 0..indent {
            self.write(b"  ")?;
        }
        Ok(())
    }

    /// `dom_xml_common_text_serialization`: `&`, `<` and `>` escaped, and
    /// in an attribute `"`, tab, line feed and carriage return too
    /// (issue 59).
    fn escaped(&mut self, content: Option<&[u8]>, attribute: bool) -> Step {
        let Some(content) = content else {
            return Ok(());
        };
        let mut last = 0;
        for (i, &byte) in content.iter().enumerate() {
            let replacement: &[u8] = match byte {
                b'&' => b"&amp;",
                b'<' => b"&lt;",
                b'>' => b"&gt;",
                b'"' if attribute => b"&quot;",
                b'\t' if attribute => b"&#9;",
                b'\n' if attribute => b"&#10;",
                b'\r' if attribute => b"&#13;",
                _ => continue,
            };
            self.write(&content[last..i])?;
            self.write(replacement)?;
            last = i + 1;
        }
        self.write(&content[last..])
    }

    /// `dom_xml_serialize_attribute_node_value`: `name="value"`, an entity
    /// reference among the value's children written as one.
    fn attribute_node_value(&mut self, attr: Node) -> Step {
        self.write(&attr.name())?;
        self.write(b"=\"")?;
        let mut at = attr.children();
        while let Some(child) = at {
            match child.kind() {
                Kind::Text => self.escaped(child.content().as_deref(), true)?,
                Kind::EntityRef => {
                    self.write(b"&")?;
                    self.escaped(Some(&child.name()), true)?;
                    self.write(b";")?;
                }
                _ => {}
            }
            at = child.next();
        }
        self.write(b"\"")
    }

    /// The whole of `dom_xml_serialize`: the XML namespace known as `xml`,
    /// the prefix index at one, and the tree walked from `node`.
    fn serialize(&mut self, node: Node, format: bool) -> Step {
        let mut map = PrefixMap::default();
        map.add(b"xml", Some(XML_NS));
        self.index = 1;
        let indent = if format { 0 } else { -1 };
        self.algorithm(node, &map, None, indent)
    }

    /// `dom_xml_serialization_algorithm`: the node, then each child by
    /// the same algorithm, php's recursion, refused where the stack has
    /// come down to its limit, as `dom_xml_serialize_check_stack_limit`
    /// refuses it. The nest is deeper than php's budget holds, at php's
    /// bytes a level and at this engine's (#3159), so both engines refuse
    /// it.
    ///
    /// ```php
    /// $doc = Dom\XMLDocument::createEmpty();
    /// $node = $doc->createElement('a');
    /// for ($i = 0; $i < 100000; $i++) { $p = $doc->createElement('a'); $p->appendChild($node); $node = $p; }
    /// $doc->appendChild($node);
    /// try { $doc->saveXml(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $doc->documentElement->innerHTML; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Maximum call stack size reached. Infinite recursion?
    /// Maximum call stack size reached. Infinite recursion?
    /// ```
    fn algorithm(
        &mut self,
        node: Node,
        map: &PrefixMap,
        namespace: Option<&[u8]>,
        indent: i32,
    ) -> Step {
        if self.req.limits().stack_overflowed() {
            return Err(Failed::Deep);
        }
        // The frame is read where `node` left it, never moved out of its
        // answer: a nest keeps one frame's bytes at each level, not two
        // (#3159).
        let made = self.node(node, map, namespace, indent);
        let frame = match &made {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(failed) => return Err(*failed),
        };
        let mut next = frame.next;
        while let Some(child) = next {
            next = child.next();
            if frame.format {
                self.indents(frame.indent)?;
            }
            self.algorithm(child, &frame.map, frame.inherited.as_deref(), frame.indent)?;
        }
        if frame.format {
            self.indents(frame.indent - 1)?;
        }
        if let Some(end) = &frame.end {
            self.write(b"</")?;
            self.qname(end)?;
            self.write(b">")?;
        }
        Ok(())
    }

    /// `dom_xml_serialization_algorithm` for one node: what it writes
    /// before its children, and the frame its children are written from.
    /// A call of its own, so what writing a node takes is off the stack
    /// of a nest written level by level, which keeps only the frame
    /// (#3159).
    #[inline(never)]
    fn node(
        &mut self,
        node: Node,
        map: &PrefixMap,
        namespace: Option<&[u8]>,
        indent: i32,
    ) -> Step<Option<Frame>> {
        let children_of = |node: Node| Frame {
            map: map.clone(),
            next: node.children(),
            inherited: namespace.map(<[u8]>::to_vec),
            indent,
            format: false,
            end: None,
        };
        match node.kind() {
            Kind::Element => return self.element(node, map, namespace, indent),
            Kind::Fragment => return Ok(Some(children_of(node))),
            Kind::Document | Kind::HtmlDocument => {
                // DOM-Parsing issue 50: the declaration libxml writes.
                if !self.dump.document_head(node) {
                    return Err(Failed::Refused);
                }
                return Ok(Some(children_of(node)));
            }
            Kind::Text => {
                let content = node.content();
                if self.well_formed
                    && let Some(content) = &content
                {
                    chars_ok(content)?;
                }
                self.escaped(content.as_deref(), false)?;
            }
            Kind::Comment => {
                let content = node.content();
                if self.well_formed
                    && let Some(content) = &content
                {
                    chars_ok(content)?;
                    if content.windows(2).any(|w| w == b"--") || content.last() == Some(&b'-') {
                        return Err(Failed::Refused);
                    }
                }
                self.write(b"<!--")?;
                if let Some(content) = &content {
                    self.write(content)?;
                }
                self.write(b"-->")?;
            }
            Kind::Pi => {
                let name = node.name();
                let content = node.content();
                if self.well_formed {
                    if name.contains(&b':') || name.eq_ignore_ascii_case(b"xml") {
                        return Err(Failed::Refused);
                    }
                    if let Some(content) = &content {
                        chars_ok(content)?;
                        if content.windows(2).any(|w| w == b"?>") {
                            return Err(Failed::Refused);
                        }
                    }
                }
                self.write(b"<?")?;
                self.write(&name)?;
                self.write(b" ")?;
                if let Some(content) = &content {
                    self.write(content)?;
                }
                self.write(b"?>")?;
            }
            Kind::CData => {
                // DOM-Parsing issue 38.
                self.write(b"<![CDATA[")?;
                if let Some(content) = node.content() {
                    self.write(&content)?;
                }
                self.write(b"]]>")?;
            }
            Kind::Attribute => {
                // DOM-Parsing issue 28: an attribute alone is written as
                // `prefix:name="value"`.
                if let Some(prefix) = node.ns().and_then(Ns::prefix) {
                    self.write(&prefix)?;
                    self.write(b":")?;
                }
                self.attribute_node_value(node)?;
            }
            kind => {
                if !self.dump.tree(node) {
                    return Err(Failed::Refused);
                }
                if kind == Kind::Dtd {
                    self.write(b"\n")?;
                }
            }
        }
        Ok(None)
    }

    /// `dom_xml_serialize_element_node`: the start tag with the namespace
    /// declarations it needs, and the frame of the children and the end
    /// tag, none where the element closes itself.
    fn element(
        &mut self,
        element: Node,
        parent_map: &PrefixMap,
        namespace: Option<&[u8]>,
        indent: i32,
    ) -> Step<Option<Frame>> {
        let name = element.name();
        if self.well_formed && !Name::valid_ncname(&name) {
            return Err(Failed::Refused);
        }
        let first = element.children();
        let format = indent >= 0 && first.is_some_and(should_format);
        self.write(b"<")?;
        let mut map = parent_map.clone();
        let mut local = LocalPrefixes::default();
        let local_default = record_namespaces(&mut map, &mut local, element);
        let mut inherited = namespace.map(<[u8]>::to_vec);
        let mut scope = Scope {
            map,
            local,
            local_default,
        };
        let (qname, ignore_namespace_definition) =
            self.start_name(element, name, &mut scope, &mut inherited)?;
        let Scope {
            mut map, mut local, ..
        } = scope;
        self.attributes(element, &mut map, &mut local, ignore_namespace_definition)?;
        let mut skip_end_tag = false;
        if first.is_none() && !self.no_empty {
            if ns_is(element, HTML_NS) {
                if XML_VOID_ELEMENTS.contains(&qname.name.as_slice()) {
                    self.write(b" /")?;
                    skip_end_tag = true;
                }
            } else {
                self.write(b"/")?;
                skip_end_tag = true;
            }
        }
        self.write(b">")?;
        if skip_end_tag {
            return Ok(None);
        }
        let next = if ns_is(element, HTML_NS) && qname.name == b"template" {
            self.private.and_then(|p| p.template(element))
        } else {
            first
        };
        Ok(Some(Frame {
            map,
            next,
            inherited,
            indent: if format { indent + 1 } else { -1 },
            format,
            end: Some(qname),
        }))
    }

    /// Steps 11 and 12 of `dom_xml_serialize_element_node`: the element's
    /// qualified name written with the declaration it needs, `inherited`
    /// set to what its children inherit, and whether its own default
    /// namespace declaration is to be left out.
    fn start_name(
        &mut self,
        element: Node,
        name: Vec<u8>,
        scope: &mut Scope,
        inherited: &mut Option<Vec<u8>>,
    ) -> Step<(QName, bool)> {
        let mut ignore_namespace_definition = false;
        let ns = element.ns().and_then(Ns::href);
        let qname;
        if *inherited == ns {
            if scope.local_default.is_some() {
                ignore_namespace_definition = true;
            }
            let prefix = ns_is(element, XML_NS).then(|| b"xml".to_vec());
            qname = QName { prefix, name };
            self.qname(&qname)?;
        } else {
            let prefix = element.ns().and_then(Ns::prefix);
            // DOM-Parsing issue 52.
            let mut candidate = if prefix.is_none()
                && equal_or_empty(ns.as_deref(), scope.local_default.as_deref())
            {
                None
            } else {
                preferred_prefix(&scope.map, &scope.local, prefix.as_deref(), ns.as_deref())
            };
            if prefix.as_deref() == Some(b"xmlns") {
                if self.well_formed {
                    return Err(Failed::Refused);
                }
                candidate.clone_from(&prefix);
            }
            if let Some(candidate) = candidate {
                qname = QName {
                    prefix: Some(candidate),
                    name,
                };
                if let Some(default) = &scope.local_default
                    && default != XML_NS
                {
                    *inherited = (!default.is_empty()).then(|| default.clone());
                }
                self.qname(&qname)?;
            } else if let Some(prefix) = prefix {
                let prefix = if scope.local.contains(&prefix) {
                    generate_prefix(
                        &mut scope.map,
                        &mut scope.local,
                        ns.as_deref(),
                        &mut self.index,
                    )
                } else {
                    scope.map.add(&prefix, ns.as_deref());
                    scope.local.add(&prefix, ns.as_deref());
                    prefix
                };
                qname = QName {
                    prefix: Some(prefix.clone()),
                    name,
                };
                self.qname(&qname)?;
                self.write(b" xmlns:")?;
                self.write(&prefix)?;
                self.write(b"=\"")?;
                self.escaped(ns.as_deref(), true)?;
                self.write(b"\"")?;
                if let Some(default) = &scope.local_default {
                    *inherited = (!default.is_empty()).then(|| default.clone());
                }
            } else if scope.local_default.is_none()
                || !equal_or_empty(scope.local_default.as_deref(), ns.as_deref())
            {
                // DOM-Parsing issue 47.
                ignore_namespace_definition = true;
                qname = QName { prefix: None, name };
                inherited.clone_from(&ns);
                self.qname(&qname)?;
                self.write(b" xmlns=\"")?;
                self.escaped(ns.as_deref(), true)?;
                self.write(b"\"")?;
            } else {
                qname = QName { prefix: None, name };
                inherited.clone_from(&ns);
                self.qname(&qname)?;
            }
        }
        Ok((qname, ignore_namespace_definition))
    }

    /// `dom_xml_serialize_attributes`: each attribute with the namespace
    /// declaration its namespace needs, an `xmlns` attribute left out where
    /// it would declare what is already declared.
    fn attributes(
        &mut self,
        element: Node,
        map: &mut PrefixMap,
        local: &mut LocalPrefixes,
        ignore_namespace_definition: bool,
    ) -> Step {
        let element_ns = element.ns().and_then(Ns::href);
        let mut localnames: Vec<(Option<Vec<u8>>, Vec<u8>)> = Vec::new();
        let mut at = element.properties();
        while let Some(attr) = at {
            at = attr.next();
            let name = attr.name();
            let attr_ns = attr.ns().and_then(Ns::href);
            let attr_prefix = attr.ns().and_then(Ns::prefix);
            if self.well_formed {
                let key = (attr_ns.clone(), name.clone());
                if localnames.contains(&key) {
                    return Err(Failed::Refused);
                }
                localnames.push(key);
            }
            let mut candidate = None;
            if let Some(attribute_namespace) = &attr_ns {
                candidate = preferred_prefix(
                    map,
                    local,
                    attr_prefix.as_deref(),
                    Some(attribute_namespace),
                );
                if attribute_namespace == XMLNS_NS {
                    let value = attribute_value(attr);
                    if value == XML_NS {
                        continue;
                    }
                    if ignore_namespace_definition
                        && attr_prefix.is_none()
                        && !equal_or_empty(element_ns.as_deref(), Some(&value))
                    {
                        continue;
                    }
                    if attr_prefix.is_some()
                        && local.find(&name) != Some(value.as_slice())
                        && map.found(&name, &value)
                    {
                        continue;
                    }
                    if attr_prefix.as_deref() == Some(b"xmlns") {
                        candidate = Some(b"xmlns".to_vec());
                    }
                    if self.well_formed
                        && (value == XMLNS_NS || (value.is_empty() && candidate.is_some()))
                    {
                        return Err(Failed::Refused);
                    }
                } else if candidate.is_none() {
                    // DOM-Parsing issue 29.
                    let prefix = match &attr_prefix {
                        Some(prefix) if !local.contains(prefix) => {
                            map.add(prefix, Some(attribute_namespace));
                            local.add(prefix, Some(attribute_namespace));
                            prefix.clone()
                        }
                        _ => {
                            generate_prefix(map, local, Some(attribute_namespace), &mut self.index)
                        }
                    };
                    self.write(b" xmlns:")?;
                    self.write(&prefix)?;
                    self.write(b"=\"")?;
                    self.escaped(Some(attribute_namespace), true)?;
                    self.write(b"\"")?;
                    candidate = Some(prefix);
                }
            }
            self.write(b" ")?;
            if let Some(candidate) = &candidate {
                self.write(candidate)?;
                self.write(b":")?;
            }
            if self.well_formed
                && (!Name::valid_ncname(&name) || (name == b"xmlns" && attr_ns.is_none()))
            {
                return Err(Failed::Refused);
            }
            self.attribute_node_value(attr)?;
        }
        Ok(())
    }
}

/// `dom_xml_serialize` over `dump`: `node` written, formatted where
/// `format`, held to well-formedness where `well_formed`; whether it
/// was, and `deep` set where the stack's limit stopped it.
#[allow(clippy::too_many_arguments)] // `dom_xml_serialize`'s own, and the flag it raises
fn serialize(
    req: &Request,
    dump: &mut Dump,
    node: Node,
    format: bool,
    well_formed: bool,
    no_empty: bool,
    private: Option<&Private>,
    deep: &mut bool,
) -> bool {
    let mut serializer = Serializer {
        req,
        dump,
        private,
        well_formed,
        no_empty,
        index: 1,
    };
    match serializer.serialize(node, format) {
        Ok(()) => true,
        Err(Failed::Refused) => false,
        Err(Failed::Deep) => {
            *deep = true;
            false
        }
    }
}

/// `dom_xml_serialize`'s refusal where the stack came down to its limit.
fn too_deep(req: &mut Request) -> Thrown {
    php_ext::raising::throw_error(
        req,
        "Error",
        "Maximum call stack size reached. Infinite recursion?",
    )
}

/// The document's private data, `get_private_data_from_node`.
fn private_of(req: &Request, object: PhpObject) -> Option<&Private> {
    crate::dom(req, object).and_then(|d| d.obj.private_data::<Private>())
}

/// `php_new_dom_dump_node_to_str_ex`: `node` serialised into `encoding`
/// with the save `options`, `None` where a step failed, and what libxml
/// raised; the namespace mapper is the one the document's object keeps
/// (`get_private_data_from_node`).
///
/// ```php
/// $x = Dom\XMLDocument::createFromString('<!DOCTYPE r [<!ENTITY e "v">]><r xmlns:a="urn:a" a:b="&e;"><a:c/>&e;<?p d?><![CDATA[<]]></r>');
/// echo $x->saveXml(), "\n";
/// ```
/// ```expect
/// <?xml version="1.0" encoding="UTF-8"?>
/// <!DOCTYPE r [
/// <!ENTITY e "v">
/// ]>
/// <r xmlns:a="urn:a" a:b="&e;"><a:c/>&e;<?p d?><![CDATA[<]]></r>
/// ```
fn dump_to_string(
    req: &mut Request,
    node: Node,
    options: c_int,
    format: bool,
    no_empty: bool,
    encoding: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, Thrown> {
    let owner = node.doc().and_then(Node::wrapper);
    let private = owner.as_ref().and_then(|o| o.private_data::<Private>());
    let mut deep = false;
    let reader = &*req;
    let (saved, raised) = Dump::serialise(encoding, f::XML_SAVE_AS_XML | options, |dump| {
        serialize(
            reader, dump, node, format, false, no_empty, private, &mut deep,
        )
    });
    php_ext_libxml::Errors::route(req, raised);
    if deep {
        return Err(too_deep(req));
    }
    Ok(saved)
}

/// `php_new_dom_dump_node_to_file`: the file opened through php's streams,
/// php's warning where it will not open, then `node` serialised into it;
/// the bytes written, `-1` where it failed.
///
/// ```php
/// $e = Dom\XMLDocument::createEmpty('1.0', 'UTF-16LE');
/// $e->append($e->createElement('r'));
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// var_dump($e->saveXmlFile($f), bin2hex(file_get_contents($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(92)
/// string(184) "3c003f0078006d006c002000760065007200730069006f006e003d00220031002e0030002200200065006e0063006f00640069006e0067003d0022005500540046002d00310036004c00450022003f003e000a003c0072002f003e00"
/// ```
fn dump_to_file(
    req: &mut Request,
    path: &[u8],
    node: Node,
    format: bool,
    no_empty: bool,
    encoding: Option<&[u8]>,
) -> Result<i64, Thrown> {
    let Some(stream) = req.open_stream(path, b"wb", false)? else {
        return Ok(-1);
    };
    let saved = match dump_to_string(req, node, 0, format, no_empty, encoding) {
        Ok(saved) => saved,
        Err(thrown) => {
            stream.free(req);
            return Err(thrown);
        }
    };
    let written = match saved {
        Some(bytes) => {
            let _ = stream.write(req, &bytes);
            stream.tell(req)
        }
        None => -1,
    };
    stream.free(req);
    Ok(written)
}

/// `php_new_dom_default_document_handlers`, the serialiser
/// `dom_set_xml_class` gives every modern document, which simplexml's
/// `asXML` reaches too.
///
/// ```php
/// $d = Dom\XMLDocument::createFromString('<root/>');
/// $s = simplexml_import_dom($d);
/// $s->addChild('b:child', 'v', 'urn:b');
/// $d->documentElement->firstElementChild->appendChild($d->createElementNS('urn:c', 'c:x'));
/// echo $s->asXML(), "\n", $s->children('urn:b')->child->asXML(), "\n";
/// ```
/// ```expect
/// <?xml version="1.0" encoding="UTF-8"?>
/// <root><b:child xmlns:b="urn:b">v<c:x xmlns:c="urn:c"/></b:child></root>
/// <b:child xmlns:b="urn:b">v<c:x xmlns:c="urn:c"/></b:child>
/// ```
pub(crate) struct Modern;

pub(crate) static MODERN: Modern = Modern;

impl php_ext_libxml::DocumentHandlers for Modern {
    fn dump_doc_to_str(
        &self,
        req: &mut Request,
        doc: Node,
        options: i32,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let format = options & f::XML_SAVE_FORMAT != 0;
        dump_to_string(req, doc, options, format, no_empty, encoding)
    }

    fn dump_node_to_str(
        &self,
        req: &mut Request,
        _: Node,
        node: Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        dump_to_string(req, node, 0, format, no_empty, encoding)
    }

    fn dump_doc_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        doc: Node,
        format: bool,
        no_empty: bool,
        encoding: Option<&[u8]>,
    ) -> Result<i64, Thrown> {
        dump_to_file(req, path, doc, format, no_empty, encoding)
    }

    fn dump_node_to_file(
        &self,
        req: &mut Request,
        path: &[u8],
        _: Node,
        node: Node,
        format: bool,
    ) -> Result<i64, Thrown> {
        dump_to_file(req, path, node, format, false, None)
    }
}

/// `dom_element_html_fragment_serialize`'s XML arm: each of `nodes`
/// serialised on its own, held to well-formedness, into UTF-8, what
/// libxml raised routed; `None` where one was not well formed, and php's
/// `Error` where the tree is deeper than the stack takes.
///
/// ```php
/// $x = Dom\XMLDocument::createFromString('<r><p:a xmlns:p="urn:p"/></r>');
/// $r = $x->documentElement;
/// $p = $r->firstChild;
/// $r->append($x->createProcessingInstruction('xml-stylesheet', 'a'));
/// var_dump($p->outerHTML, $r->innerHTML);
/// $r->append($x->createComment('-'));
/// try { var_dump($r->innerHTML); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(22) "<p:a xmlns:p="urn:p"/>"
/// string(42) "<p:a xmlns:p="urn:p"/><?xml-stylesheet a?>"
/// The resulting XML serialization is not well-formed
/// ```
pub(crate) fn well_formed(
    req: &mut Request,
    object: PhpObject,
    nodes: &[Node],
) -> Result<Option<Vec<u8>>, Thrown> {
    let mut deep = false;
    let reader = &*req;
    let private = private_of(reader, object);
    let (saved, raised) = Dump::serialise(Some(b"UTF-8"), f::XML_SAVE_AS_XML, |dump| {
        nodes
            .iter()
            .all(|node| serialize(reader, dump, *node, false, true, false, private, &mut deep))
    });
    php_ext_libxml::Errors::route(req, raised);
    if deep {
        return Err(too_deep(req));
    }
    Ok(saved)
}

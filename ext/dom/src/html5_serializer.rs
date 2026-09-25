//! `html5_serializer.c`: the HTML fragment serialisation algorithm
//! (<https://html.spec.whatwg.org/multipage/parsing.html#serialising-html-fragments>,
//! 2024-12-11) as php writes it, and `html_document.c`'s
//! `Dom\HTMLDocument::saveHtml` and `saveHtmlFile` over it
//! (`dom_common_save`): the serialiser writes UTF-8, which php hands on
//! with each invalid sequence replaced, or re-encodes into the document's
//! encoding through lexbor's encoder with `?` for what it lacks.

use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::{Request, Stream};
use php_ext::values::{PhpObject, Value};
use php_ext_lexbor::ffi::encoding::{
    DECODE_CONTINUE, Decoder, Encoder, Encoding, MAX_CODEPOINT, REPLACEMENT_BYTES, Status,
};
use php_ext_libxml::ffi::{Kind, Node, Ns};

use crate::document::{XML_NS, XMLNS_NS};
use crate::object::Code;
use crate::private::{HTML_NS, MATHML_NS, Private, SVG_NS, XLINK_NS, ns_is};
use crate::{Entry, arity, dom_throw, fetch, object_arg, path_arg, string, this_node};

/// Where the serialisation's pieces go, each piece written as it is made
/// (`write_string_len`); `false` where the sink refused one.
pub(crate) trait Sink {
    fn write(&mut self, bytes: &[u8]) -> bool;
}

/// `smart_str`: the bytes kept.
impl Sink for Vec<u8> {
    fn write(&mut self, bytes: &[u8]) -> bool {
        self.extend_from_slice(bytes);
        true
    }
}

/// `dom_write_output_stream`: each piece written to the stream.
struct StreamSink<'r> {
    req: &'r mut Request,
    stream: Stream,
}

impl Sink for StreamSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> bool {
        bytes.is_empty() || self.stream.write(self.req, bytes).is_some()
    }
}

/// `dom_output_ctx`: the pieces decoded as UTF-8 and written in the
/// document's encoding.
struct Encoded<'o> {
    out: &'o mut dyn Sink,
    decoder: Decoder,
    encoder: Encoder,
    /// The fast path, `dom_saveHTML_write_string_len_utf8_output`.
    utf_8: bool,
}

/// The size of `dom_common_save`'s buffers.
const BUFFER: usize = 4096;

impl<'o> Encoded<'o> {
    /// `dom_common_save`'s set-up: a UTF-8 decoder, and an encoder into
    /// `encoding` writing U+FFFD for what UTF-8 lacks and `?` for what
    /// another encoding lacks.
    fn new(out: &'o mut dyn Sink, encoding: Encoding, utf_8: Encoding) -> Encoded<'o> {
        let replacement: &'static [u8] = if encoding.is_utf_8() {
            REPLACEMENT_BYTES
        } else {
            b"?"
        };
        Encoded {
            out,
            decoder: Decoder::new(utf_8, BUFFER),
            encoder: Encoder::new(encoding, BUFFER, replacement),
            utf_8: encoding.is_utf_8(),
        }
    }

    /// `dom_saveHTML_write_string_len_utf8_output`: the bytes as they are,
    /// each invalid sequence replaced by U+FFFD, a sequence the piece ends
    /// inside left to the next.
    fn write_utf_8(&mut self, bytes: &[u8]) -> bool {
        self.decoder.set_status(Status::Ok);
        let mut rest = bytes;
        let mut last = 0;
        while !rest.is_empty() {
            let before = bytes.len() - rest.len();
            let cp = self.decoder.utf_8_single(&mut rest);
            if cp > MAX_CODEPOINT {
                if !self.out.write(&bytes[last..before]) {
                    return false;
                }
                if cp == DECODE_CONTINUE {
                    self.decoder.set_status(Status::Continue);
                    return true;
                }
                if !self.out.write(REPLACEMENT_BYTES) {
                    return false;
                }
                last = bytes.len() - rest.len();
            }
        }
        last == bytes.len() || self.out.write(&bytes[last..])
    }

    /// `dom_saveHTML_write_string_len`: decoded, encoded, and written a
    /// buffer at a time.
    fn write_encoded(&mut self, bytes: &[u8]) -> bool {
        let mut rest = bytes;
        loop {
            let decoded = self.decoder.decode(&mut rest);
            let codepoints = self.decoder.codepoints().to_vec();
            let mut cps = codepoints.as_slice();
            loop {
                let encoded = self.encoder.encode(&mut cps);
                if !self.out.write(self.encoder.bytes()) {
                    return false;
                }
                self.encoder.clear();
                if encoded != Status::SmallBuffer {
                    break;
                }
            }
            self.decoder.clear();
            if decoded != Status::SmallBuffer {
                return true;
            }
        }
    }

    /// The end of `dom_common_save`: what the decoder still holds encoded
    /// and written, then the encoder finished and what its buffer holds
    /// written, the earlier bytes among them as php writes them.
    fn finish(&mut self) -> bool {
        self.decoder.finish();
        if !self.decoder.codepoints().is_empty() {
            let codepoints = self.decoder.codepoints().to_vec();
            let mut cps = codepoints.as_slice();
            self.encoder.encode(&mut cps);
            if !self.out.write(self.encoder.bytes()) {
                return false;
            }
        }
        self.encoder.finish();
        self.encoder.bytes().is_empty() || self.out.write(self.encoder.bytes())
    }
}

impl Sink for Encoded<'_> {
    fn write(&mut self, bytes: &[u8]) -> bool {
        if self.utf_8 {
            self.write_utf_8(bytes)
        } else {
            self.write_encoded(bytes)
        }
    }
}

/// The template contents under a subtree, looked up once before the
/// serialisation starts so the sink may borrow the request
/// (`php_dom_retrieve_templated_content`).
#[derive(Default)]
pub(crate) struct Templates(Vec<(Node, Node)>);

impl Templates {
    /// The contents of every template element under `root`, and under
    /// those contents, from the document's private data.
    pub(crate) fn under(private: Option<&Private>, root: Node) -> Templates {
        let mut found = Vec::new();
        let Some(private) = private.filter(|p| p.template_count() > 0) else {
            return Templates(found);
        };
        let mut visit = |node: Node, roots: &mut Vec<Node>| {
            if node.kind() == Kind::Element
                && let Some(content) = private.template(node)
                && !found.iter().any(|(e, _)| *e == node)
            {
                found.push((node, content));
                roots.push(content);
            }
        };
        let mut roots = Vec::new();
        visit(root, &mut roots);
        roots.push(root);
        while let Some(base) = roots.pop() {
            let mut at = base.children();
            while let Some(node) = at {
                visit(node, &mut roots);
                at = crate::next_in_tree_order(node, base);
            }
        }
        Templates(found)
    }

    fn get(&self, element: Node) -> Option<Node> {
        self.0.iter().find(|(e, _)| *e == element).map(|(_, c)| *c)
    }
}

/// A piece the sink refused.
struct Refused;

type Step = Result<(), Refused>;

/// The HTML elements that serialise as void
/// (<https://html.spec.whatwg.org/multipage/parsing.html#serializes-as-void>).
const VOID_ELEMENTS: [&[u8]; 18] = [
    b"area",
    b"base",
    b"br",
    b"col",
    b"embed",
    b"hr",
    b"img",
    b"input",
    b"link",
    b"meta",
    b"source",
    b"track",
    b"wbr",
    b"basefont",
    b"bgsound",
    b"frame",
    b"keygen",
    b"param",
];

/// The HTML elements whose text php writes as it is.
const RAW_TEXT_ELEMENTS: [&[u8]; 7] = [
    b"style",
    b"script",
    b"xmp",
    b"iframe",
    b"noembed",
    b"noframes",
    b"plaintext",
];

/// `dom_html5_serializes_as_void`.
fn serializes_as_void(node: Node) -> bool {
    ns_is(node, HTML_NS) && VOID_ELEMENTS.contains(&node.name().as_slice())
}

struct Serializer<'a> {
    out: &'a mut dyn Sink,
    templates: &'a Templates,
}

impl Serializer<'_> {
    fn write(&mut self, bytes: &[u8]) -> Step {
        if bytes.is_empty() || self.out.write(bytes) {
            Ok(())
        } else {
            Err(Refused)
        }
    }

    /// `dom_html5_escape_string`: `&` and U+00A0 escaped, and `"` in an
    /// attribute or `<` and `>` outside one.
    fn escaped(&mut self, content: &[u8], attribute: bool) -> Step {
        let mut last = 0;
        let mut i = 0;
        while i < content.len() {
            let (replacement, width): (&[u8], usize) = match content[i] {
                b'&' => (b"&amp;", 1),
                0xC2 if content.get(i + 1) == Some(&0xA0) => (b"&nbsp;", 2),
                b'"' if attribute => (b"&quot;", 1),
                b'<' if !attribute => (b"&lt;", 1),
                b'>' if !attribute => (b"&gt;", 1),
                _ => {
                    i += 1;
                    continue;
                }
            };
            self.write(&content[last..i])?;
            self.write(replacement)?;
            i += width;
            last = i;
        }
        self.write(&content[last..])
    }

    /// `dom_html5_serialize_text_node`: the text of a raw text element as
    /// it is, any other escaped.
    fn text(&mut self, node: Node) -> Step {
        let Some(content) = node.content() else {
            return Ok(());
        };
        if let Some(parent) = node.parent()
            && parent.kind() == Kind::Element
            && ns_is(parent, HTML_NS)
            && RAW_TEXT_ELEMENTS.contains(&parent.name().as_slice())
        {
            return self.write(&content);
        }
        self.escaped(&content, false)
    }

    /// `dom_html5_serialize_element_tag_name`: the prefix written for a
    /// namespace other than HTML, `MathML` and SVG.
    fn tag_name(&mut self, node: Node) -> Step {
        if let Some(prefix) = node.ns().and_then(Ns::prefix)
            && !(ns_is(node, HTML_NS) || ns_is(node, MATHML_NS) || ns_is(node, SVG_NS))
        {
            self.write(&prefix)?;
            self.write(b":")?;
        }
        self.write(&node.name())
    }

    /// `dom_html5_serialize_element_start`.
    fn start_tag(&mut self, node: Node) -> Step {
        self.write(b"<")?;
        self.tag_name(node)?;
        let mut at = node.properties();
        while let Some(attr) = at {
            at = attr.next();
            self.write(b" ")?;
            let name = attr.name();
            match attr.ns() {
                None => self.write(&name)?,
                Some(ns) => {
                    if ns_is(attr, XML_NS) {
                        self.write(b"xml:")?;
                        self.write(&name)?;
                    } else if ns_is(attr, XMLNS_NS) {
                        if name == b"xmlns" {
                            self.write(b"xmlns")?;
                        } else {
                            self.write(b"xmlns:")?;
                            self.write(&name)?;
                        }
                    } else if ns_is(attr, XLINK_NS) {
                        self.write(b"xlink:")?;
                        self.write(&name)?;
                    } else if let Some(prefix) = ns.prefix() {
                        self.write(&prefix)?;
                        self.write(b":")?;
                        self.write(&name)?;
                    } else {
                        self.write(&name)?;
                    }
                }
            }
            self.write(b"=\"")?;
            let mut child = attr.children();
            while let Some(c) = child {
                match c.kind() {
                    Kind::Text => {
                        if let Some(content) = c.content() {
                            self.escaped(&content, true)?;
                        }
                    }
                    Kind::EntityRef => {
                        self.write(b"&")?;
                        self.escaped(&c.name(), true)?;
                        self.write(b";")?;
                    }
                    _ => {}
                }
                child = c.next();
            }
            self.write(b"\"")?;
        }
        self.write(b">")
    }

    /// `dom_html5_serialize_element_end`.
    fn end_tag(&mut self, node: Node) -> Step {
        if serializes_as_void(node) {
            return Ok(());
        }
        self.write(b"</")?;
        self.tag_name(node)?;
        self.write(b">")
    }

    /// `dom_html5_serialize_node`: from `start` on through its following
    /// siblings (none where `start` is `only`) and their descendants, up
    /// to `bound`.
    fn nodes(&mut self, start: Option<Node>, bound: Option<Node>, only: Option<Node>) -> Step {
        let Some(mut node) = start else {
            return Ok(());
        };
        let next_of = |n: Node| if Some(n) == only { None } else { n.next() };
        loop {
            match node.kind() {
                Kind::Dtd => {
                    self.write(b"<!DOCTYPE ")?;
                    self.write(&node.name())?;
                    self.write(b">")?;
                }
                Kind::CData | Kind::Text => self.text(node)?,
                Kind::Pi => {
                    self.write(b"<?")?;
                    self.write(&node.name())?;
                    self.write(b" ")?;
                    if let Some(content) = node.content() {
                        self.write(&content)?;
                    }
                    self.write(b">")?;
                }
                Kind::Comment => {
                    self.write(b"<!--")?;
                    if let Some(content) = node.content() {
                        self.write(&content)?;
                    }
                    self.write(b"-->")?;
                }
                Kind::Element => {
                    self.start_tag(node)?;
                    let children = if ns_is(node, HTML_NS) && node.name() == b"template" {
                        self.templates.get(node)
                    } else {
                        node.children()
                    };
                    match children {
                        Some(first) => {
                            if !serializes_as_void(node) {
                                node = first;
                                continue;
                            }
                        }
                        None => self.end_tag(node)?,
                    }
                }
                Kind::Fragment => {
                    if let Some(first) = node.children() {
                        node = first;
                        continue;
                    }
                }
                Kind::EntityRef => {
                    self.write(b"&")?;
                    self.write(&node.name())?;
                    self.write(b";")?;
                }
                _ => {}
            }
            if let Some(next) = next_of(node) {
                node = next;
                continue;
            }
            loop {
                let parent = node.parent();
                if parent == bound {
                    return Ok(());
                }
                let Some(parent) = parent else {
                    return Ok(());
                };
                node = parent;
                if node.kind() == Kind::Element {
                    self.end_tag(node)?;
                }
                if let Some(next) = next_of(node) {
                    node = next;
                    break;
                }
            }
        }
    }
}

/// `dom_html5_serialize`: the children of `node`, the contents of a
/// template, nothing for a void element or a node that holds no children.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!DOCTYPE html><body><template><b>x</b></template><img></body>', LIBXML_NOERROR);
/// $body = $d->body;
/// $t = $body->firstChild;
/// $img = $body->lastChild;
/// var_dump($body->innerHTML, $t->innerHTML, $img->innerHTML);
/// ```
/// ```expect
/// string(34) "<template><b>x</b></template><img>"
/// string(8) "<b>x</b>"
/// string(0) ""
/// ```
pub(crate) fn inner(out: &mut dyn Sink, templates: &Templates, node: Node) -> bool {
    if !matches!(
        node.kind(),
        Kind::Element | Kind::Fragment | Kind::Document | Kind::HtmlDocument
    ) || (node.kind() == Kind::Element && serializes_as_void(node))
    {
        return true;
    }
    let children = templates.get(node).or_else(|| node.children());
    let mut serializer = Serializer { out, templates };
    serializer.nodes(children, Some(node), None).is_ok()
}

/// `dom_html5_serialize_outer`: the node itself, a document's or a
/// fragment's children.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!DOCTYPE html><p x:y="1" xml:lang="en">a<svg xlink:href="#"><rect/></svg></p>', LIBXML_NOERROR);
/// $p = $d->body->firstChild;
/// echo $d->saveHtml(), "\n", $p->outerHTML, "\n", $d->saveHtml($d->createDocumentFragment()), "|\n";
/// ```
/// ```expect
/// <!DOCTYPE html><html><head></head><body><p x:y="1" xml:lang="en">a<svg xlink:href="#"><rect></rect></svg></p></body></html>
/// <p x:y="1" xml:lang="en">a<svg xlink:href="#"><rect></rect></svg></p>
/// |
/// ```
pub(crate) fn outer(out: &mut dyn Sink, templates: &Templates, node: Node) -> bool {
    let mut serializer = Serializer { out, templates };
    if matches!(
        node.kind(),
        Kind::Document | Kind::HtmlDocument | Kind::Fragment
    ) {
        let Some(first) = node.children() else {
            return true;
        };
        return serializer.nodes(Some(first), first.parent(), None).is_ok();
    }
    serializer
        .nodes(Some(node), node.parent(), Some(node))
        .is_ok()
}

/// `dom_common_save`: `node` serialised into `out` in `encoding`, an
/// encoding lexbor does not know read as UTF-8.
fn common_save(out: &mut dyn Sink, templates: &Templates, node: Node, encoding: &[u8]) -> bool {
    let Some(utf_8) = Encoding::utf_8() else {
        return false;
    };
    let encoding = Encoding::by_name(encoding).unwrap_or(utf_8);
    let mut encoded = Encoded::new(out, encoding, utf_8);
    outer(&mut encoded, templates, node) && encoded.finish()
}

/// The template contents under `node` of the document `object` stands for.
///
/// ```php
/// $d = Dom\HTMLDocument::createFromString('<!DOCTYPE html><template id="t"><i>1</i><template><u>2</u></template></template>', LIBXML_NOERROR);
/// $t = $d->head->firstChild;
/// echo $t->outerHTML, "\n", $d->saveXml($t), "\n";
/// ```
/// ```expect
/// <template id="t"><i>1</i><template><u>2</u></template></template>
/// <template xmlns="http://www.w3.org/1999/xhtml" id="t"><i>1</i><template><u>2</u></template></template>
/// ```
pub(crate) fn templates_of(req: &Request, object: PhpObject, node: Node) -> Templates {
    let private = crate::dom(req, object).and_then(|d| d.obj.private_data::<Private>());
    Templates::under(private, node)
}

/// `Dom\HTMLDocument::saveHtml(?Dom\Node $node = null): string`.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $html = $d->appendChild($d->createElement('html'));
/// $p = $html->appendChild($d->createElement('p'));
/// $p->setAttribute('title', "a\"b&\u{a0}<");
/// $p->append("x<y> & \u{a0}", $d->createElement('br'), $d->createComment('c'));
/// $s = $html->appendChild($d->createElement('script'));
/// $s->append('if (a < b) {}');
/// echo $d->saveHtml(), "\n", $d->saveHtml($p), "\n";
/// $x = Dom\XMLDocument::createEmpty();
/// try { $d->saveHtml($x->createElement('q')); } catch (DOMException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// <html><p title="a&quot;b&amp;&nbsp;<">x&lt;y&gt; &amp; &nbsp;<br><!--c--></p><script>if (a < b) {}</script></html>
/// <p title="a&quot;b&amp;&nbsp;<">x&lt;y&gt; &amp; &nbsp;<br><!--c--></p>
/// Wrong Document Error
/// ```
fn save_html(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let given = object_arg(req, 1)?;
    let (this, doc) = this_node(req)?;
    let node = match given {
        Some(given) => {
            let node = fetch(req, given)?;
            if node.doc() != Some(doc) {
                return Err(dom_throw(req, Code::WrongDocument));
            }
            node
        }
        None => doc,
    };
    let templates = templates_of(req, this, node);
    let encoding = doc.doc_encoding().unwrap_or_default();
    let mut bytes = Vec::new();
    common_save(&mut bytes, &templates, node, &encoding);
    string(req, &bytes)
}

/// `Dom\HTMLDocument::saveHtmlFile(string $filename): int|false`: the
/// document written through php's streams, each piece as it is made.
///
/// ```php
/// $d = Dom\HTMLDocument::createEmpty();
/// $d->appendChild($d->createElement('p'))->append("\u{e9}");
/// $f = tempnam(sys_get_temp_dir(), 'dom');
/// var_dump($d->saveHtmlFile($f), file_get_contents($f));
/// unlink($f);
/// try { $d->saveHtmlFile(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(9)
/// string(9) "<p>é</p>"
/// Dom\HTMLDocument::saveHtmlFile(): Argument #1 ($filename) must not be empty
/// ```
fn save_html_file(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let file = path_arg(req, 1)?;
    if file.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let Some(stream) = req.open_stream(&file, b"wb", false)? else {
        return Ok(Value::bool(false));
    };
    let (this, doc) = match this_node(req) {
        Ok(found) => found,
        Err(thrown) => {
            stream.close(req);
            return Err(thrown);
        }
    };
    let templates = templates_of(req, this, doc);
    let encoding = doc.doc_encoding().unwrap_or_default();
    let saved = {
        let mut out = StreamSink { req, stream };
        common_save(&mut out, &templates, doc, &encoding)
    };
    if !saved {
        stream.close(req);
        return Ok(Value::bool(false));
    }
    let written = stream.tell(req);
    stream.close(req);
    Ok(Value::int(written))
}

/// The methods, `Class::method` to entry.
pub(crate) const METHODS: &[(&str, Entry)] = &[
    ("Dom\\HTMLDocument::saveHtml", save_html),
    ("Dom\\HTMLDocument::saveHtmlFile", save_html_file),
];

//! libxml2's `xmlTextReader` (`xmlreader.h`), which php's xmlreader
//! drives call for call (design §4.10): the reader, the memory
//! input and the `RelaxNG` schema php keeps beside it, and each call that
//! may parse wrapped in php's error capture with the request lent, so a
//! document php's wrappers opened is read through them.

use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};

use php_ext::raising::Thrown;
use php_ext::request::{Request, Stream};

use super::io::sanitized_globals;
use super::{Alloc, Node, Raised, Routing, cstr, loader, sys};

/// `xmlreader_object`'s three libxml members: the reader, the memory
/// input `XMLReader::XML` hands it, and the `RelaxNG` schema it validates
/// against.
pub struct TextReader {
    raw: NonNull<sys::xmlTextReader>,
    input: Option<NonNull<sys::xmlParserInputBuffer>>,
    schema: Option<NonNull<c_void>>,
}

// SAFETY: the reader and what it holds are reached only from the request
// that made them, one thread at a time (ADR-0005).
unsafe impl Send for TextReader {}

impl Drop for TextReader {
    fn drop(&mut self) {
        // SAFETY: the reader this owns, freed once.
        unsafe { sys::xmlFreeTextReader(self.raw.as_ptr()) };
        if let Some(input) = self.input.take() {
            // SAFETY: the buffer this owns, which the freed reader read.
            unsafe { sys::xmlFreeParserInputBuffer(input.as_ptr()) };
        }
        if let Some(schema) = self.schema.take() {
            // SAFETY: the schema this owns, which the freed reader held.
            unsafe { sys::xmlRelaxNGFree(schema.as_ptr()) };
        }
    }
}

/// Where a `RelaxNG` schema is read from, `XMLREADER_LOAD_FILE` or
/// `XMLREADER_LOAD_STRING`.
#[derive(Debug, Clone, Copy)]
pub enum RelaxNg<'a> {
    File(&'a [u8]),
    Source(&'a [u8]),
}

/// What `XMLReader::expand` came to.
#[derive(Debug, Clone, Copy)]
pub enum Expanded {
    /// `xmlTextReaderExpand` answered no node.
    Failed,
    /// `xmlDocCopyNode` could not copy the node it answered.
    Uncopyable,
    /// The copy, in the document it was asked for or in none.
    Copy(Node),
}

/// The errors of a reader call are its parser's, raised through the
/// handlers libxml gave that parser, which php never replaced.
const ROUTING: Routing = Routing::Parser {
    sax: ptr::null_mut(),
};

fn held(s: Option<&[u8]>) -> Option<Vec<u8>> {
    s.map(cstr)
}

fn at(s: Option<&Vec<u8>>) -> *const u8 {
    s.map_or(ptr::null(), Vec::as_ptr)
}

impl TextReader {
    fn of(raw: *mut sys::xmlTextReader) -> Option<TextReader> {
        NonNull::new(raw).map(|raw| TextReader {
            raw,
            input: None,
            schema: None,
        })
    }

    /// `xmlReaderForFile(path, encoding, options)`, the file opened through
    /// `req`'s wrappers.
    pub fn file(
        req: &mut Request,
        path: &[u8],
        encoding: Option<&[u8]>,
        options: i32,
    ) -> (Option<TextReader>, Vec<Raised>, Option<Thrown>) {
        let (p, e) = (cstr(path), held(encoding));
        Raised::capture_io(req, Some(ROUTING), || {
            let raw = sanitized_globals(|| {
                // SAFETY: a NUL-terminated path and an encoding or null.
                unsafe { sys::xmlReaderForFile(p.as_ptr().cast(), at(e.as_ref()).cast(), options) }
            });
            TextReader::of(raw)
        })
    }

    /// `xml_reader_from_string`'s reader: `xmlNewTextReader` over a copy
    /// of `source` in memory, with `base` as its URI, then
    /// `xmlTextReaderSetup` with the encoding and options.
    pub fn memory(
        req: &mut Request,
        source: &[u8],
        base: Option<&[u8]>,
        encoding: Option<&[u8]>,
        options: i32,
    ) -> (Option<TextReader>, Vec<Raised>, Option<Thrown>) {
        let (u, e) = (held(base), held(encoding));
        let Ok(len) = c_int::try_from(source.len()) else {
            return (None, Vec::new(), None);
        };
        Raised::capture_io(req, Some(ROUTING), || {
            sanitized_globals(|| {
                // SAFETY: `len` readable bytes, copied into the buffer.
                let input = unsafe {
                    sys::xmlParserInputBufferCreateMem(
                        source.as_ptr().cast(),
                        len,
                        sys::XML_CHAR_ENCODING_NONE,
                    )
                };
                let input = NonNull::new(input)?;
                // SAFETY: a live buffer the reader reads, and a URI or null.
                let raw = unsafe { sys::xmlNewTextReader(input.as_ptr(), at(u.as_ref()).cast()) };
                let Some(mut reader) = TextReader::of(raw) else {
                    // SAFETY: the buffer no reader took, freed once.
                    unsafe { sys::xmlFreeParserInputBuffer(input.as_ptr()) };
                    return None;
                };
                reader.input = Some(input);
                // SAFETY: the live reader, no new input, and strings or null.
                let set = unsafe {
                    sys::xmlTextReaderSetup(
                        raw,
                        ptr::null_mut(),
                        at(u.as_ref()).cast(),
                        at(e.as_ref()).cast(),
                        options,
                    )
                };
                (set == 0).then_some(reader)
            })
        })
    }

    /// `xmlReaderForIO` over `stream`, which the reader reads through and
    /// never closes; its first bytes are read here, with `req` lent.
    pub fn stream(
        req: &mut Request,
        stream: Stream,
        uri: Option<&[u8]>,
        encoding: Option<&[u8]>,
        options: i32,
    ) -> (Option<TextReader>, Vec<Raised>, Option<Thrown>) {
        let (u, e) = (held(uri), held(encoding));
        let context = loader::lend(req, stream);
        Raised::capture_io(req, Some(ROUTING), || {
            let raw = sanitized_globals(|| {
                // SAFETY: the callbacks read the box this hands over, which
                // the reader owns from here and closes even where it fails.
                unsafe {
                    sys::xmlReaderForIO(
                        loader::stream_read,
                        Some(loader::stream_release),
                        context,
                        at(u.as_ref()).cast(),
                        at(e.as_ref()).cast(),
                        options,
                    )
                }
            });
            TextReader::of(raw)
        })
    }

    /// `f` on the reader with `req` lent to libxml's callbacks.
    fn call<R>(
        &mut self,
        req: &mut Request,
        f: impl FnOnce(*mut sys::xmlTextReader) -> R,
    ) -> (R, Vec<Raised>, Option<Thrown>) {
        let raw = self.raw.as_ptr();
        Raised::capture_io(req, Some(ROUTING), || f(raw))
    }

    /// `xmlFreeTextReader` with `req` lent, so what its close reads or
    /// closes goes through the request.
    pub fn close(self, req: &mut Request) -> Vec<Raised> {
        let ((), raised, _thrown) = Raised::capture_io(req, Some(ROUTING), || drop(self));
        raised
    }

    /// `xmlTextReaderRead`.
    pub fn read(&mut self, req: &mut Request) -> (i32, Vec<Raised>, Option<Thrown>) {
        // SAFETY: the live reader.
        self.call(req, |r| unsafe { sys::xmlTextReaderRead(r) })
    }

    /// `xmlTextReaderNext`.
    pub fn next(&mut self, req: &mut Request) -> (i32, Vec<Raised>, Option<Thrown>) {
        // SAFETY: the live reader.
        self.call(req, |r| unsafe { sys::xmlTextReaderNext(r) })
    }

    /// `xmlTextReaderReadInnerXml`.
    pub fn read_inner_xml(
        &mut self,
        req: &mut Request,
    ) -> (Option<Vec<u8>>, Vec<Raised>, Option<Thrown>) {
        self.call(req, |r| {
            // SAFETY: the live reader.
            let p = unsafe { sys::xmlTextReaderReadInnerXml(r) };
            // SAFETY: a string libxml allocated, given back once.
            unsafe { Alloc::take(p) }
        })
    }

    /// `xmlTextReaderReadOuterXml`.
    pub fn read_outer_xml(
        &mut self,
        req: &mut Request,
    ) -> (Option<Vec<u8>>, Vec<Raised>, Option<Thrown>) {
        self.call(req, |r| {
            // SAFETY: the live reader.
            let p = unsafe { sys::xmlTextReaderReadOuterXml(r) };
            // SAFETY: a string libxml allocated, given back once.
            unsafe { Alloc::take(p) }
        })
    }

    /// `xmlTextReaderReadString`.
    pub fn read_string(
        &mut self,
        req: &mut Request,
    ) -> (Option<Vec<u8>>, Vec<Raised>, Option<Thrown>) {
        self.call(req, |r| {
            // SAFETY: the live reader.
            let p = unsafe { sys::xmlTextReaderReadString(r) };
            // SAFETY: a string libxml allocated, given back once.
            unsafe { Alloc::take(p) }
        })
    }

    /// `xmlTextReaderExpand`, and `xmlDocCopyNode(node, doc, 1)` of what it
    /// answered.
    pub fn expand(
        &mut self,
        req: &mut Request,
        doc: Option<Node>,
    ) -> (Expanded, Vec<Raised>, Option<Thrown>) {
        let doc = doc.map_or(ptr::null_mut(), |d| d.raw().cast::<sys::xmlDoc>());
        self.call(req, |r| {
            // SAFETY: the live reader.
            let node = unsafe { sys::xmlTextReaderExpand(r) };
            if node.is_null() {
                return Expanded::Failed;
            }
            // SAFETY: the reader's live node, and a live document or null.
            let copy = unsafe { sys::xmlDocCopyNode(node, doc, 1) };
            Node::from_raw(copy).map_or(Expanded::Uncopyable, Expanded::Copy)
        })
    }

    /// `xmlTextReaderSchemaValidate(reader, xsd)`, the schema opened through
    /// `req`'s wrappers; `None` unsets it.
    pub fn schema_validate(
        &mut self,
        req: &mut Request,
        xsd: Option<&[u8]>,
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        let x = held(xsd);
        self.call(req, |r| {
            sanitized_globals(|| {
                // SAFETY: the live reader and a NUL-terminated path or null.
                unsafe { sys::xmlTextReaderSchemaValidate(r, at(x.as_ref()).cast()) }
            })
        })
    }

    /// `php_xmlreader_set_relaxng_schema`'s libxml half: the schema parsed
    /// (`_xmlreader_get_relaxNG`) and set, or with `None` unset, the
    /// schema kept where libxml took it; `0` where it did.
    pub fn relaxng(
        &mut self,
        req: &mut Request,
        from: Option<RelaxNg<'_>>,
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        let (ret, raised, thrown) = self.call(req, |r| {
            let schema = match from {
                None => ptr::null_mut(),
                Some(from) => {
                    let parser = match from {
                        RelaxNg::File(path) => {
                            let p = cstr(path);
                            // SAFETY: a NUL-terminated path.
                            unsafe { sys::xmlRelaxNGNewParserCtxt(p.as_ptr().cast()) }
                        }
                        RelaxNg::Source(source) => {
                            let len = c_int::try_from(source.len()).unwrap_or(c_int::MAX);
                            // SAFETY: `len` readable bytes the parser copies.
                            unsafe { sys::xmlRelaxNGNewMemParserCtxt(source.as_ptr().cast(), len) }
                        }
                    };
                    if parser.is_null() {
                        return (-1, None);
                    }
                    let schema = sanitized_globals(|| {
                        // SAFETY: a live schema parser.
                        let schema = unsafe { sys::xmlRelaxNGParse(parser) };
                        // SAFETY: the parser, freed once.
                        unsafe { sys::xmlRelaxNGFreeParserCtxt(parser) };
                        schema
                    });
                    if schema.is_null() {
                        return (-1, None);
                    }
                    schema
                }
            };
            // SAFETY: the live reader and a schema or null.
            let ret = unsafe { sys::xmlTextReaderRelaxNGSetSchema(r, schema) };
            (ret, NonNull::new(schema))
        });
        let (ret, schema) = ret;
        if ret == 0 {
            if let Some(old) = std::mem::replace(&mut self.schema, schema) {
                // SAFETY: the schema this held, which libxml let go.
                unsafe { sys::xmlRelaxNGFree(old.as_ptr()) };
            }
        } else if let Some(schema) = schema {
            // SAFETY: the schema libxml did not take, freed once.
            unsafe { sys::xmlRelaxNGFree(schema.as_ptr()) };
        }
        (ret, raised, thrown)
    }

    /// `xmlTextReaderGetParserProp`.
    #[must_use]
    pub fn parser_prop(&self, prop: i32) -> i32 {
        // SAFETY: the live reader.
        unsafe { sys::xmlTextReaderGetParserProp(self.raw.as_ptr(), prop) }
    }

    /// `xmlTextReaderSetParserProp`.
    pub fn set_parser_prop(&mut self, prop: i32, value: bool) -> i32 {
        // SAFETY: the live reader.
        unsafe { sys::xmlTextReaderSetParserProp(self.raw.as_ptr(), prop, c_int::from(value)) }
    }

    /// `xmlTextReaderGetAttributeNo`.
    #[must_use]
    pub fn attribute_no(&self, no: i32) -> Option<Vec<u8>> {
        // SAFETY: the live reader.
        let p = unsafe { sys::xmlTextReaderGetAttributeNo(self.raw.as_ptr(), no) };
        // SAFETY: a string libxml allocated, given back once.
        unsafe { Alloc::take(p) }
    }

    /// `xmlTextReaderMoveToAttributeNo`.
    pub fn move_to_attribute_no(&mut self, no: i32) -> i32 {
        // SAFETY: the live reader.
        unsafe { sys::xmlTextReaderMoveToAttributeNo(self.raw.as_ptr(), no) }
    }
}

/// The reader's calls that take strings and answer one libxml allocated,
/// each the `xmlTextReader*` entry named.
macro_rules! owned {
    ($($(#[$doc:meta])* $name:ident => $c:ident($($arg:ident),*);)*) => {
        impl TextReader {
            $(
                $(#[$doc])*
                #[must_use]
                pub fn $name(&self, $($arg: &[u8]),*) -> Option<Vec<u8>> {
                    $(let $arg = cstr($arg);)*
                    // SAFETY: the live reader and NUL-terminated strings.
                    let p = unsafe { sys::$c(self.raw.as_ptr() $(, $arg.as_ptr())*) };
                    // SAFETY: a string libxml allocated, given back once.
                    unsafe { Alloc::take(p) }
                }
            )*
        }
    };
}

owned! {
    /// `xmlTextReaderGetAttribute`.
    attribute => xmlTextReaderGetAttribute(name);
    /// `xmlTextReaderGetAttributeNs`.
    attribute_ns => xmlTextReaderGetAttributeNs(name, ns);
    /// `xmlTextReaderLookupNamespace`.
    lookup_namespace => xmlTextReaderLookupNamespace(prefix);
}

/// The reader's calls that answer a number, each the `xmlTextReader*`
/// entry named.
macro_rules! numbers {
    ($($(#[$doc:meta])* $name:ident => $c:ident($($arg:ident),*);)*) => {
        impl TextReader {
            $(
                $(#[$doc])*
                pub fn $name(&mut self, $($arg: &[u8]),*) -> i32 {
                    $(let $arg = cstr($arg);)*
                    // SAFETY: the live reader and NUL-terminated strings.
                    unsafe { sys::$c(self.raw.as_ptr() $(, $arg.as_ptr())*) }
                }
            )*
        }
    };
}

numbers! {
    /// `xmlTextReaderMoveToAttribute`.
    move_to_attribute => xmlTextReaderMoveToAttribute(name);
    /// `xmlTextReaderMoveToAttributeNs`.
    move_to_attribute_ns => xmlTextReaderMoveToAttributeNs(name, ns);
    /// `xmlTextReaderMoveToElement`.
    move_to_element => xmlTextReaderMoveToElement();
    /// `xmlTextReaderMoveToFirstAttribute`.
    move_to_first_attribute => xmlTextReaderMoveToFirstAttribute();
    /// `xmlTextReaderMoveToNextAttribute`.
    move_to_next_attribute => xmlTextReaderMoveToNextAttribute();
    /// `xmlTextReaderIsValid`.
    is_valid => xmlTextReaderIsValid();
    /// `xmlTextReaderAttributeCount`.
    attribute_count => xmlTextReaderAttributeCount();
    /// `xmlTextReaderDepth`.
    depth => xmlTextReaderDepth();
    /// `xmlTextReaderHasAttributes`.
    has_attributes => xmlTextReaderHasAttributes();
    /// `xmlTextReaderHasValue`.
    has_value => xmlTextReaderHasValue();
    /// `xmlTextReaderIsDefault`.
    is_default => xmlTextReaderIsDefault();
    /// `xmlTextReaderIsEmptyElement`.
    is_empty_element => xmlTextReaderIsEmptyElement();
    /// `xmlTextReaderNodeType`.
    node_type => xmlTextReaderNodeType();
}

/// The reader's calls that answer a string it keeps, copied, each the
/// `xmlTextReaderConst*` entry named.
macro_rules! borrowed {
    ($($(#[$doc:meta])* $name:ident => $c:ident;)*) => {
        impl TextReader {
            $(
                $(#[$doc])*
                #[must_use]
                pub fn $name(&self) -> Option<Vec<u8>> {
                    // SAFETY: the live reader.
                    let p = unsafe { sys::$c(self.raw.as_ptr()) };
                    // SAFETY: the reader's string, live until it moves on.
                    (!p.is_null()).then(|| unsafe { Alloc::bytes(p) }.to_vec())
                }
            )*
        }
    };
}

borrowed! {
    /// `xmlTextReaderConstBaseUri`.
    base_uri => xmlTextReaderConstBaseUri;
    /// `xmlTextReaderConstLocalName`.
    local_name => xmlTextReaderConstLocalName;
    /// `xmlTextReaderConstName`.
    name => xmlTextReaderConstName;
    /// `xmlTextReaderConstNamespaceUri`.
    namespace_uri => xmlTextReaderConstNamespaceUri;
    /// `xmlTextReaderConstPrefix`.
    prefix => xmlTextReaderConstPrefix;
    /// `xmlTextReaderConstValue`.
    value => xmlTextReaderConstValue;
    /// `xmlTextReaderConstXmlLang`.
    xml_lang => xmlTextReaderConstXmlLang;
}

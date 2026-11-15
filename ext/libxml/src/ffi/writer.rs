//! libxml2's `xmlTextWriter` (`xmlwriter.h`), which php's xmlwriter
//! drives call for call (design §4.10): the emitter, where its bytes
//! go, and each of its calls wrapped in php's error capture with the
//! request lent, so an emitter whose buffer php's wrappers opened writes
//! through them whenever libxml flushes.

use std::ffi::{c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use php_ext::raising::Thrown;
use php_ext::request::{Request, Stream};

use super::{Raised, cstr, loader, sys};

/// `xmlTextWriterPtr`, owned, beside the memory its buffer writes into
/// where it writes into memory (`ze_xmlwriter_object`'s `output`).
pub struct Emitter {
    raw: NonNull<sys::xmlTextWriter>,
    memory: Option<NonNull<Vec<u8>>>,
}

// SAFETY: the writer and its buffer are reached only from the request that
// made them, one thread at a time (ADR-0005).
unsafe impl Send for Emitter {}

impl Drop for Emitter {
    fn drop(&mut self) {
        // SAFETY: the writer this owns, freed once; it closes its buffer.
        unsafe { sys::xmlFreeTextWriter(self.raw.as_ptr()) };
        if let Some(memory) = self.memory.take() {
            // SAFETY: the box `memory` leaked, which the closed buffer no
            // longer reaches.
            drop(unsafe { Box::from_raw(memory.as_ptr()) });
        }
    }
}

/// `xml_writer_stream_write_memory`: libxml's bytes appended.
unsafe extern "C" fn memory_write(
    context: *mut c_void,
    buffer: *const c_char,
    len: c_int,
) -> c_int {
    let Ok(n) = usize::try_from(len) else {
        return -1;
    };
    // SAFETY: the vector `Emitter::memory` leaked, live while the buffer is.
    let into = unsafe { &mut *context.cast::<Vec<u8>>() };
    // SAFETY: libxml hands `len` bytes at `buffer`.
    into.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer.cast::<u8>(), n) });
    len
}

/// The close of a buffer whose `ioctx` the emitter owns: a memory target's
/// vector, which the emitter drops itself, or a stream it only borrows
/// (`xml_writer_stream_close` deletes its own reference and closes nothing).
unsafe extern "C" fn keep_close(_context: *mut c_void) -> c_int {
    0
}

impl Emitter {
    fn over(buffer: *mut sys::xmlOutputBuffer) -> Option<NonNull<sys::xmlTextWriter>> {
        if buffer.is_null() {
            return None;
        }
        // SAFETY: a fresh buffer the writer takes.
        let raw = NonNull::new(unsafe { sys::xmlNewTextWriter(buffer) });
        if raw.is_none() {
            // SAFETY: the buffer no writer took, closed once.
            unsafe { sys::xmlOutputBufferClose(buffer) };
        }
        raw
    }

    /// `xml_writer_create_in_memory`: an emitter over a buffer that appends
    /// to memory this owns.
    #[must_use]
    pub fn memory() -> Option<Emitter> {
        let memory = NonNull::from(Box::leak(Box::<Vec<u8>>::default()));
        // SAFETY: the two callbacks and a context that outlives the buffer.
        let buffer = unsafe {
            sys::xmlOutputBufferCreateIO(
                memory_write,
                Some(keep_close),
                memory.as_ptr().cast(),
                ptr::null_mut(),
            )
        };
        let Some(raw) = Emitter::over(buffer) else {
            // SAFETY: the box leaked above, which no buffer reaches.
            drop(unsafe { Box::from_raw(memory.as_ptr()) });
            return None;
        };
        Some(Emitter {
            raw,
            memory: Some(memory),
        })
    }

    /// `XMLWriter::toStream`: an emitter over a buffer that writes through
    /// `stream`, holding one owner of it and never closing it.
    pub fn stream(
        req: &mut Request,
        stream: Stream,
    ) -> (Option<Emitter>, Vec<Raised>, Option<Thrown>) {
        let context = loader::lend(req, stream);
        Raised::capture_io(req, None, || {
            // SAFETY: the callbacks read the box `lend` made; the buffer
            // owns it from here and closes it even where it fails.
            let buffer = unsafe {
                sys::xmlOutputBufferCreateIO(
                    loader::stream_write,
                    Some(loader::stream_release),
                    context,
                    ptr::null_mut(),
                )
            };
            if buffer.is_null() {
                // SAFETY: the box no buffer took.
                unsafe { loader::stream_release(context) };
                return None;
            }
            Emitter::over(buffer).map(|raw| Emitter { raw, memory: None })
        })
    }

    /// `xmlNewTextWriterFilename(path, 0)`, the path opened for writing
    /// through `req`'s wrappers.
    pub fn uri(req: &mut Request, path: &[u8]) -> (Option<Emitter>, Vec<Raised>, Option<Thrown>) {
        let p = cstr(path);
        Raised::capture_io(req, None, || {
            // SAFETY: a NUL-terminated path, no compression.
            let raw = NonNull::new(unsafe { sys::xmlNewTextWriterFilename(p.as_ptr().cast(), 0) });
            raw.map(|raw| Emitter { raw, memory: None })
        })
    }

    /// The bytes the memory target holds, `None` for any other target.
    #[must_use]
    pub fn buffered(&self) -> Option<&[u8]> {
        // SAFETY: the vector this owns, which libxml writes only inside a
        // call on the writer.
        self.memory.map(|m| unsafe { m.as_ref() }.as_slice())
    }

    /// `smart_str_extract`: the memory target's bytes, taken.
    pub fn take_buffered(&mut self) -> Vec<u8> {
        // SAFETY: as above.
        self.memory
            .map(|mut m| std::mem::take(unsafe { m.as_mut() }))
            .unwrap_or_default()
    }

    /// `f` on the emitter with `req` lent to libxml's callbacks: what it
    /// answered, what libxml raised, and what a program's code threw from
    /// a stream it wrote to.
    fn call(
        &mut self,
        req: &mut Request,
        f: impl FnOnce(*mut sys::xmlTextWriter) -> c_int,
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        let raw = self.raw.as_ptr();
        Raised::capture_io(req, None, || f(raw))
    }

    /// `xmlFreeTextWriter` with `req` lent, so what its close flushes
    /// reaches the stream it writes to.
    pub fn close(self, req: &mut Request) -> (Vec<Raised>, Option<Thrown>) {
        let ((), raised, thrown) = Raised::capture_io(req, None, || drop(self));
        (raised, thrown)
    }

    /// `xmlTextWriterSetIndent`.
    pub fn set_indent(
        &mut self,
        req: &mut Request,
        on: bool,
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        // SAFETY: the live writer.
        self.call(req, |w| unsafe {
            sys::xmlTextWriterSetIndent(w, c_int::from(on))
        })
    }

    /// `xmlTextWriterStartDTDEntity`.
    pub fn start_dtd_entity(
        &mut self,
        req: &mut Request,
        parameter: bool,
        name: &[u8],
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        let n = cstr(name);
        // SAFETY: the live writer and a NUL-terminated name.
        self.call(req, |w| unsafe {
            sys::xmlTextWriterStartDTDEntity(w, c_int::from(parameter), n.as_ptr())
        })
    }

    /// `xmlTextWriterWriteDTDEntity`.
    #[allow(clippy::too_many_arguments)]
    pub fn write_dtd_entity(
        &mut self,
        req: &mut Request,
        parameter: bool,
        name: &[u8],
        public_id: Option<&[u8]>,
        system_id: Option<&[u8]>,
        ndata: Option<&[u8]>,
        content: &[u8],
    ) -> (i32, Vec<Raised>, Option<Thrown>) {
        let texts = [Some(name), public_id, system_id, ndata, Some(content)].map(held);
        let [name, public_id, system_id, ndata, content] =
            texts.each_ref().map(Option::as_ref).map(at);
        let pe = c_int::from(parameter);
        // SAFETY: the live writer and NUL-terminated strings or null.
        self.call(req, |w| unsafe {
            sys::xmlTextWriterWriteDTDEntity(w, pe, name, public_id, system_id, ndata, content)
        })
    }
}

/// A string as C takes it, NUL-terminated, or none.
fn held(s: Option<&[u8]>) -> Option<Vec<u8>> {
    s.map(cstr)
}

/// The pointer [`held`] hands C.
fn at(s: Option<&Vec<u8>>) -> *const u8 {
    s.map_or(ptr::null(), Vec::as_ptr)
}

/// The emitter's calls that take strings alone, each one `xmlTextWriter*`
/// entry of the same name, its strings `NULL` where `None`.
macro_rules! calls {
    ($($(#[$doc:meta])* $name:ident => $c:ident($($arg:ident),*);)*) => {
        impl Emitter {
            $(
                $(#[$doc])*
                pub fn $name(
                    &mut self,
                    req: &mut Request,
                    $($arg: Option<&[u8]>),*
                ) -> (i32, Vec<Raised>, Option<Thrown>) {
                    $(let $arg = held($arg);)*
                    // SAFETY: the live writer and NUL-terminated strings or null.
                    self.call(req, |w| unsafe { sys::$c(w $(, at($arg.as_ref()).cast())*) })
                }
            )*
        }
    };
}

calls! {
    /// `xmlTextWriterFlush`: the bytes written, `-1` on failure.
    flush => xmlTextWriterFlush();
    /// `xmlTextWriterSetIndentString`.
    set_indent_string => xmlTextWriterSetIndentString(indent);
    /// `xmlTextWriterStartDocument`.
    start_document => xmlTextWriterStartDocument(version, encoding, standalone);
    /// `xmlTextWriterEndDocument`.
    end_document => xmlTextWriterEndDocument();
    /// `xmlTextWriterStartComment`.
    start_comment => xmlTextWriterStartComment();
    /// `xmlTextWriterEndComment`.
    end_comment => xmlTextWriterEndComment();
    /// `xmlTextWriterWriteComment`.
    write_comment => xmlTextWriterWriteComment(content);
    /// `xmlTextWriterStartElement`.
    start_element => xmlTextWriterStartElement(name);
    /// `xmlTextWriterStartElementNS`.
    start_element_ns => xmlTextWriterStartElementNS(prefix, name, uri);
    /// `xmlTextWriterEndElement`.
    end_element => xmlTextWriterEndElement();
    /// `xmlTextWriterFullEndElement`.
    full_end_element => xmlTextWriterFullEndElement();
    /// `xmlTextWriterWriteElement`.
    write_element => xmlTextWriterWriteElement(name, content);
    /// `xmlTextWriterWriteElementNS`.
    write_element_ns => xmlTextWriterWriteElementNS(prefix, name, uri, content);
    /// `xmlTextWriterWriteRaw`.
    write_raw => xmlTextWriterWriteRaw(content);
    /// `xmlTextWriterWriteString`.
    write_string => xmlTextWriterWriteString(content);
    /// `xmlTextWriterStartAttribute`.
    start_attribute => xmlTextWriterStartAttribute(name);
    /// `xmlTextWriterStartAttributeNS`.
    start_attribute_ns => xmlTextWriterStartAttributeNS(prefix, name, uri);
    /// `xmlTextWriterEndAttribute`.
    end_attribute => xmlTextWriterEndAttribute();
    /// `xmlTextWriterWriteAttribute`.
    write_attribute => xmlTextWriterWriteAttribute(name, content);
    /// `xmlTextWriterWriteAttributeNS`.
    write_attribute_ns => xmlTextWriterWriteAttributeNS(prefix, name, uri, content);
    /// `xmlTextWriterStartPI`.
    start_pi => xmlTextWriterStartPI(target);
    /// `xmlTextWriterEndPI`.
    end_pi => xmlTextWriterEndPI();
    /// `xmlTextWriterWritePI`.
    write_pi => xmlTextWriterWritePI(target, content);
    /// `xmlTextWriterStartCDATA`.
    start_cdata => xmlTextWriterStartCDATA();
    /// `xmlTextWriterEndCDATA`.
    end_cdata => xmlTextWriterEndCDATA();
    /// `xmlTextWriterWriteCDATA`.
    write_cdata => xmlTextWriterWriteCDATA(content);
    /// `xmlTextWriterStartDTD`.
    start_dtd => xmlTextWriterStartDTD(name, public_id, system_id);
    /// `xmlTextWriterEndDTD`.
    end_dtd => xmlTextWriterEndDTD();
    /// `xmlTextWriterWriteDTD`.
    write_dtd => xmlTextWriterWriteDTD(name, public_id, system_id, subset);
    /// `xmlTextWriterStartDTDElement`.
    start_dtd_element => xmlTextWriterStartDTDElement(name);
    /// `xmlTextWriterEndDTDElement`.
    end_dtd_element => xmlTextWriterEndDTDElement();
    /// `xmlTextWriterWriteDTDElement`.
    write_dtd_element => xmlTextWriterWriteDTDElement(name, content);
    /// `xmlTextWriterStartDTDAttlist`.
    start_dtd_attlist => xmlTextWriterStartDTDAttlist(name);
    /// `xmlTextWriterEndDTDAttlist`.
    end_dtd_attlist => xmlTextWriterEndDTDAttlist();
    /// `xmlTextWriterWriteDTDAttlist`.
    write_dtd_attlist => xmlTextWriterWriteDTDAttlist(name, content);
    /// `xmlTextWriterEndDTDEntity`.
    end_dtd_entity => xmlTextWriterEndDTDEntity();
}

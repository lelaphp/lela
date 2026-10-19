//! lexbor's encodings (`lexbor/encoding`, `lexbor/html/encoding.h`), as
//! `html_document.c` drives them: an [`Encoding`] named by label, by the
//! pre-scan's name or by id; a streaming [`Decoder`] into code points and
//! an [`Encoder`] out of them, each owning its buffer; the UTF-8
//! single-code-point steps php's fast paths take; and [`prescan`], the
//! `<meta>` pre-scan of `dom_determine_encoding`.

use std::ptr::{self, NonNull};

use super::{Lent, sys};

/// `LXB_ENCODING_REPLACEMENT_BYTES`: U+FFFD in UTF-8.
pub const REPLACEMENT_BYTES: &[u8] = b"\xEF\xBF\xBD";
/// `LXB_ENCODING_REPLACEMENT_CODEPOINT`.
pub const REPLACEMENT_CODEPOINT: u32 = 0xFFFD;
/// `LXB_ENCODING_MAX_CODEPOINT`: a decoded code point above it is a signal.
pub const MAX_CODEPOINT: u32 = 0x0010_FFFF;
/// `LXB_ENCODING_DECODE_ERROR`: the bytes read were not a code point.
pub const DECODE_ERROR: u32 = 0x001F_FFFF;
/// `LXB_ENCODING_DECODE_CONTINUE`: the input ended inside a sequence.
pub const DECODE_CONTINUE: u32 = 0x002F_FFFF;

/// `LXB_ENCODING_REPLACEMENT_BUFFER`, what a decoder writes for a bad
/// sequence.
static REPLACEMENT_BUFFER: [u32; 1] = [REPLACEMENT_CODEPOINT];

/// What a step of lexbor answered (`lexbor_status_t`), as the loops php
/// writes over it read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// `LXB_STATUS_OK`.
    Ok,
    /// `LXB_STATUS_CONTINUE`: more input is needed to finish a sequence.
    Continue,
    /// `LXB_STATUS_SMALL_BUFFER`: the buffer written into filled; drain it and
    /// call again.
    SmallBuffer,
    /// Any other status, by its number.
    Failed(u32),
}

impl Status {
    pub(crate) fn from_raw(raw: sys::lxb_status_t) -> Status {
        match raw {
            sys::LXB_STATUS_OK => Status::Ok,
            sys::LXB_STATUS_CONTINUE => Status::Continue,
            sys::LXB_STATUS_SMALL_BUFFER => Status::SmallBuffer,
            other => Status::Failed(other),
        }
    }

    fn raw(self) -> sys::lxb_status_t {
        match self {
            Status::Ok => sys::LXB_STATUS_OK,
            Status::Continue => sys::LXB_STATUS_CONTINUE,
            Status::SmallBuffer => sys::LXB_STATUS_SMALL_BUFFER,
            Status::Failed(n) => n,
        }
    }
}

/// One of lexbor's encodings: `const lxb_encoding_data_t *`, a table the
/// library holds for the life of the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Encoding(NonNull<sys::lxb_encoding_data_t>);

// SAFETY: the table is static data lexbor never writes after load.
unsafe impl Send for Encoding {}
// SAFETY: as above.
unsafe impl Sync for Encoding {}

impl Encoding {
    /// `lxb_encoding_data_by_name`: a WHATWG label, case and surrounding
    /// whitespace ignored.
    #[must_use]
    pub fn by_name(name: &[u8]) -> Option<Encoding> {
        // SAFETY: a pointer and length of live bytes; the answer is static.
        let data = unsafe { sys::lxb_encoding_data_by_name(name.as_ptr(), name.len()) };
        NonNull::new(data.cast_mut()).map(Encoding)
    }

    /// `lxb_encoding_data_by_pre_name`: the name the `<meta>` pre-scan
    /// found.
    #[must_use]
    pub fn by_pre_name(name: &[u8]) -> Option<Encoding> {
        // SAFETY: as in `by_name`.
        let data = unsafe { sys::lxb_encoding_data_by_pre_name(name.as_ptr(), name.len()) };
        NonNull::new(data.cast_mut()).map(Encoding)
    }

    fn by_id(id: std::ffi::c_uint) -> Option<Encoding> {
        // SAFETY: an id of lexbor's table; the answer is static or null.
        let data = unsafe { sys::lxb_encoding_data_noi(id) };
        NonNull::new(data.cast_mut()).map(Encoding)
    }

    /// `lxb_encoding_data(LXB_ENCODING_UTF_8)`.
    #[must_use]
    pub fn utf_8() -> Option<Encoding> {
        Encoding::by_id(sys::LXB_ENCODING_UTF_8)
    }

    /// `lxb_encoding_data(LXB_ENCODING_UTF_16LE)`.
    #[must_use]
    pub fn utf_16le() -> Option<Encoding> {
        Encoding::by_id(sys::LXB_ENCODING_UTF_16LE)
    }

    /// `lxb_encoding_data(LXB_ENCODING_UTF_16BE)`.
    #[must_use]
    pub fn utf_16be() -> Option<Encoding> {
        Encoding::by_id(sys::LXB_ENCODING_UTF_16BE)
    }

    fn data(self) -> &'static sys::lxb_encoding_data_t {
        // SAFETY: a table of the library, static and never written.
        unsafe { &*self.0.as_ptr() }
    }

    /// The encoding's canonical name, `encoding_data->name`: `"UTF-8"`,
    /// `"windows-1252"`.
    #[must_use]
    pub fn name(self) -> &'static [u8] {
        let name = self.data().name;
        if name.is_null() {
            return b"";
        }
        // SAFETY: a static NUL-terminated string of the table.
        unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_bytes()
    }

    /// Whether this is UTF-8, `encoding_data->encoding == LXB_ENCODING_UTF_8`.
    #[must_use]
    pub fn is_utf_8(self) -> bool {
        self.data().encoding == sys::LXB_ENCODING_UTF_8
    }
}

/// `lxb_encoding_decode_t` with the buffer it writes code points into:
/// bytes of one encoding in, code points out.
pub struct Decoder {
    ctx: Box<sys::lxb_encoding_decode_t>,
    buffer: Box<[u32]>,
    encoding: Encoding,
}

// SAFETY: the context points only at its own buffer and at static tables.
unsafe impl Send for Decoder {}

impl Decoder {
    /// `lxb_encoding_decode_init` over a buffer of `capacity` code points,
    /// with `LXB_ENCODING_REPLACEMENT_BUFFER` set for a bad sequence, as
    /// `dom_decoding_encoding_ctx_init` and
    /// `dom_setup_parser_encoding_manually` set it.
    #[must_use]
    pub fn new(encoding: Encoding, capacity: usize) -> Decoder {
        let mut buffer = vec![0u32; capacity.max(1)].into_boxed_slice();
        let mut ctx = Box::new(blank_decode());
        // SAFETY: a context and a buffer this owns; lexbor keeps the
        // buffer's pointer, which moves with neither box.
        unsafe {
            sys::lxb_encoding_decode_init_noi(
                &raw mut *ctx,
                encoding.0.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        // SAFETY: a static one-element buffer.
        unsafe {
            sys::lxb_encoding_decode_replace_set_noi(
                &raw mut *ctx,
                REPLACEMENT_BUFFER.as_ptr(),
                REPLACEMENT_BUFFER.len(),
            )
        };
        Decoder {
            ctx,
            buffer,
            encoding,
        }
    }

    /// `lxb_encoding_decode_init_single`: a decoder for
    /// [`Decoder::utf_8_single`] alone, with no buffer.
    #[must_use]
    pub fn single(encoding: Encoding) -> Decoder {
        let mut ctx = Box::new(blank_decode());
        // SAFETY: a context this owns.
        unsafe { sys::lxb_encoding_decode_init_single_noi(&raw mut *ctx, encoding.0.as_ptr()) };
        Decoder {
            ctx,
            buffer: Box::new([]),
            encoding,
        }
    }

    /// The encoding this decodes.
    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// `decode_data->decode(&decode, &buf_ref, buf_end)`: bytes read from
    /// the front of `input`, which is left at what was not read.
    pub fn decode(&mut self, input: &mut &[u8]) -> Status {
        let decode = self.encoding.data().decode;
        let mut at = input.as_ptr();
        let end = input.as_ptr_range().end;
        // SAFETY: a live context, and `at..end` the live bytes of `input`.
        let status = unsafe { decode(&raw mut *self.ctx, &raw mut at, end) };
        *input = advance(input, at);
        Status::from_raw(status)
    }

    /// The code points written since the buffer was last cleared,
    /// `lxb_encoding_decode_buf_used`.
    #[must_use]
    pub fn codepoints(&self) -> &[u32] {
        let used = self.ctx.buffer_used.min(self.buffer.len());
        &self.buffer[..used]
    }

    /// `lxb_encoding_decode_buf_used_set(&decode, 0)`.
    pub fn clear(&mut self) {
        // SAFETY: a live context.
        unsafe { sys::lxb_encoding_decode_buf_used_set_noi(&raw mut *self.ctx, 0) };
    }

    /// `lxb_encoding_decode_finish`: a sequence cut off by the end of the
    /// input written as the replacement.
    pub fn finish(&mut self) -> Status {
        // SAFETY: a live context.
        Status::from_raw(unsafe { sys::lxb_encoding_decode_finish_noi(&raw mut *self.ctx) })
    }

    /// `lxb_encoding_decode_utf_8_single`: one code point read from the
    /// front of `input`, or [`DECODE_ERROR`] or [`DECODE_CONTINUE`].
    pub fn utf_8_single(&mut self, input: &mut &[u8]) -> u32 {
        let mut at = input.as_ptr();
        let end = input.as_ptr_range().end;
        // SAFETY: a live context, and `at..end` the live bytes of `input`.
        let cp =
            unsafe { sys::lxb_encoding_decode_utf_8_single(&raw mut *self.ctx, &raw mut at, end) };
        *input = advance(input, at);
        cp
    }

    /// `decode.status`, which php's fast path sets to `CONTINUE` when a
    /// chunk ends inside a sequence.
    #[must_use]
    pub fn status(&self) -> Status {
        Status::from_raw(self.ctx.status)
    }

    /// Set `decode.status`.
    pub fn set_status(&mut self, status: Status) {
        self.ctx.status = status.raw();
    }

    /// `decode.u.utf_8.need`: the bytes the UTF-8 decoder still waits for.
    #[must_use]
    pub fn utf_8_need(&self) -> u32 {
        self.ctx.u[0]
    }
}

/// `lxb_encoding_encode_t` with the buffer it writes bytes into: code
/// points in, bytes of one encoding out.
pub struct Encoder {
    ctx: Box<sys::lxb_encoding_encode_t>,
    buffer: Box<[u8]>,
    encoding: Encoding,
}

// SAFETY: the context points only at its own buffer, at static tables and
// at the static replacement it was given.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `lxb_encoding_encode_init` over a buffer of `capacity` bytes, with
    /// `replacement` written for a code point the encoding lacks
    /// (`lxb_encoding_encode_replace_set`).
    #[must_use]
    pub fn new(encoding: Encoding, capacity: usize, replacement: &'static [u8]) -> Encoder {
        let mut buffer = vec![0u8; capacity.max(1)].into_boxed_slice();
        let mut ctx = Box::new(blank_encode());
        // SAFETY: a context and a buffer this owns; lexbor keeps the
        // buffer's pointer, which moves with neither box.
        unsafe {
            sys::lxb_encoding_encode_init_noi(
                &raw mut *ctx,
                encoding.0.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        // SAFETY: a static replacement.
        unsafe {
            sys::lxb_encoding_encode_replace_set_noi(
                &raw mut *ctx,
                replacement.as_ptr(),
                replacement.len(),
            )
        };
        Encoder {
            ctx,
            buffer,
            encoding,
        }
    }

    /// The encoding this writes.
    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// `encode_data->encode(&encode, &cps, end)`: code points read from the
    /// front of `input`, which is left at what was not read.
    pub fn encode(&mut self, input: &mut &[u32]) -> Status {
        let encode = self.encoding.data().encode;
        let mut at = input.as_ptr();
        let end = input.as_ptr_range().end;
        // SAFETY: a live context, and `at..end` the live code points of
        // `input`.
        let status = unsafe { encode(&raw mut *self.ctx, &raw mut at, end) };
        let read = (at as usize).saturating_sub(input.as_ptr() as usize) / size_of::<u32>();
        *input = input.get(read..).unwrap_or_default();
        Status::from_raw(status)
    }

    /// The bytes written since the buffer was last cleared,
    /// `lxb_encoding_encode_buf_used`.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        let used = self.ctx.buffer_used.min(self.buffer.len());
        &self.buffer[..used]
    }

    /// `lxb_encoding_encode_buf_used_set(&encode, 0)`.
    pub fn clear(&mut self) {
        // SAFETY: a live context.
        unsafe { sys::lxb_encoding_encode_buf_used_set_noi(&raw mut *self.ctx, 0) };
    }

    /// `lxb_encoding_encode_finish`: ISO-2022-JP's return to ASCII.
    pub fn finish(&mut self) -> Status {
        // SAFETY: a live context.
        Status::from_raw(unsafe { sys::lxb_encoding_encode_finish_noi(&raw mut *self.ctx) })
    }
}

impl Encoder {
    /// `lxb_encoding_encode_utf_8_single`: a code point's UTF-8 bytes,
    /// `None` where it is none (php writes the replacement bytes then).
    #[must_use]
    pub fn utf_8_single(cp: u32) -> Option<Vec<u8>> {
        encode_utf_8_single(cp)
    }
}

impl Encoding {
    /// The `<meta>` pre-scan of `dom_determine_encoding`:
    /// `lxb_html_encoding_determine` over `bytes` (php hands it at most the
    /// first 1,024), its first entry named through
    /// `lxb_encoding_data_by_pre_name`; `None` where it finds none, or
    /// none lexbor knows.
    #[must_use]
    pub fn prescan(_: &Lent, bytes: &[u8]) -> Option<Encoding> {
        prescan(bytes)
    }
}

fn encode_utf_8_single(cp: u32) -> Option<Vec<u8>> {
    let utf_8 = Encoding::utf_8()?;
    let mut ctx = blank_encode();
    // SAFETY: a context on this frame.
    unsafe { sys::lxb_encoding_encode_init_single_noi(&raw mut ctx, utf_8.0.as_ptr()) };
    let mut out = [0u8; 4];
    let mut at = out.as_mut_ptr();
    let end = out.as_ptr_range().end;
    // SAFETY: a live context and the four bytes of `out`.
    let written =
        unsafe { sys::lxb_encoding_encode_utf_8_single(&raw mut ctx, &raw mut at, end, cp) };
    let n = usize::try_from(written).ok().filter(|n| *n <= out.len())?;
    Some(out[..n].to_vec())
}

fn prescan(bytes: &[u8]) -> Option<Encoding> {
    let mut em = sys::lxb_html_encoding_t {
        cache: blank_array(),
        found: blank_array(),
    };
    // SAFETY: a zeroed object on this frame.
    let status = unsafe { sys::lxb_html_encoding_init(&raw mut em) };
    if status != sys::LXB_STATUS_OK {
        return None;
    }
    let range = bytes.as_ptr_range();
    // SAFETY: an initialised object and the live bytes of `bytes`.
    let status = unsafe { sys::lxb_html_encoding_determine(&raw mut em, range.start, range.end) };
    let found = if status == sys::LXB_STATUS_OK {
        first_entry(&em.found)
    } else {
        None
    };
    // SAFETY: an initialised object, destroyed once and not freed itself.
    unsafe { sys::lxb_html_encoding_destroy(&raw mut em, false) };
    found
}

/// `lxb_html_encoding_meta_entry(&em, 0)` named through
/// `lxb_encoding_data_by_pre_name`, read before the entries are destroyed.
fn first_entry(found: &sys::lexbor_array_obj_t) -> Option<Encoding> {
    if found.length == 0 || found.list.is_null() {
        return None;
    }
    let entry = found.list.cast::<sys::lxb_html_encoding_entry_t>();
    // SAFETY: the array holds at least one entry of this type at its head.
    let entry = unsafe { &*entry };
    let len = (entry.end as usize).saturating_sub(entry.name as usize);
    // SAFETY: the name lexbor found spans `name..end`.
    let data = unsafe { sys::lxb_encoding_data_by_pre_name(entry.name, len) };
    NonNull::new(data.cast_mut()).map(Encoding)
}

/// What is left of `input` once its reader stopped at `at`.
fn advance(input: &[u8], at: *const u8) -> &[u8] {
    let read = (at as usize).saturating_sub(input.as_ptr() as usize);
    input.get(read..).unwrap_or_default()
}

fn blank_array() -> sys::lexbor_array_obj_t {
    sys::lexbor_array_obj_t {
        list: ptr::null_mut(),
        size: 0,
        length: 0,
        struct_size: 0,
    }
}

fn blank_decode() -> sys::lxb_encoding_decode_t {
    sys::lxb_encoding_decode_t {
        encoding_data: ptr::null(),
        buffer_out: ptr::null_mut(),
        buffer_length: 0,
        buffer_used: 0,
        replace_to: ptr::null(),
        replace_len: 0,
        codepoint: 0,
        second_codepoint: 0,
        prepend: false,
        have_error: false,
        status: 0,
        u: [0; 4],
    }
}

fn blank_encode() -> sys::lxb_encoding_encode_t {
    sys::lxb_encoding_encode_t {
        encoding_data: ptr::null(),
        buffer_out: ptr::null_mut(),
        buffer_length: 0,
        buffer_used: 0,
        replace_to: ptr::null(),
        replace_len: 0,
        state: 0,
    }
}

//! libmbfl as `build.rs` compiles it: the encoding and language tables,
//! the per-encoding decoder and encoder `mbstring.c` drives, and the
//! three whole-string calls it makes, each as a safe type. The structs
//! are laid out as `mbfl/mbfl_encoding.h`, `mbfl/mbfl_language.h` and
//! `mbfl/mbfl_string.h` lay them out.

use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::ptr;

type ToWchar =
    unsafe extern "C" fn(*mut *mut u8, *mut usize, *mut u32, usize, *mut c_uint) -> usize;
type FromWchar = unsafe extern "C" fn(*mut u32, usize, *mut ConvertBuf, bool);
type Check = unsafe extern "C" fn(*mut u8, usize) -> bool;
type Cut = unsafe extern "C" fn(*mut u8, usize, usize, *mut u8) -> *mut ZendString;

/// `mbfl_encoding`.
#[repr(C)]
struct RawEncoding {
    no_encoding: c_int,
    name: *const c_char,
    mime_name: *const c_char,
    aliases: *const *const c_char,
    mblen_table: *const [u8; 256],
    flag: c_uint,
    input_filter: *const c_void,
    output_filter: *const c_void,
    to_wchar: Option<ToWchar>,
    from_wchar: Option<FromWchar>,
    check: Option<Check>,
    cut: Option<Cut>,
}

/// `mbfl_language`.
#[repr(C)]
struct RawLanguage {
    no_language: c_int,
    name: *const c_char,
    short_name: *const c_char,
    aliases: *const *const c_char,
    mail_charset: c_int,
    mail_header_encoding: c_int,
    mail_body_encoding: c_int,
}

/// `mb_convert_buf`.
#[repr(C)]
struct ConvertBuf {
    out: *mut u8,
    limit: *mut u8,
    state: u32,
    errors: u32,
    replacement_char: u32,
    error_mode: c_uint,
    str: *mut ZendString,
}

/// `zend_string` as the shim lays it out, php's.
#[repr(C)]
struct ZendString {
    refcount: u32,
    type_info: u32,
    h: u64,
    len: usize,
    val: [u8; 1],
}

/// `mbfl_string`.
#[repr(C)]
struct MbflString {
    encoding: *const RawEncoding,
    val: *mut u8,
    len: usize,
}

unsafe extern "C" {
    fn mbfl_name2encoding_ex(name: *const c_char, len: usize) -> *const RawEncoding;
    safe fn mbfl_no2encoding(no: c_int) -> *const RawEncoding;
    safe fn mbfl_get_supported_encodings() -> *const *const RawEncoding;
    fn mbfl_name2language(name: *const c_char) -> *const RawLanguage;
    fn mb_fast_convert(
        input: *mut u8,
        len: usize,
        from: *const RawEncoding,
        to: *const RawEncoding,
        replacement: u32,
        mode: c_uint,
        errors: *mut c_uint,
    ) -> *mut ZendString;
    fn mbfl_strcut(
        input: *mut MbflString,
        result: *mut MbflString,
        from: usize,
        len: usize,
    ) -> *mut MbflString;
    fn mb_convert_kana_codepoint(
        c: u32,
        next: u32,
        consumed: *mut bool,
        second: *mut u32,
        mode: c_uint,
    ) -> u32;
    fn mbs_buf_init(buf: *mut ConvertBuf, size: usize, replacement: u32, mode: c_uint);
    fn mbs_buf_free(buf: *mut ConvertBuf);
    fn mbs_buf_len(buf: *mut ConvertBuf) -> usize;
    fn mbs_buf_bytes(buf: *mut ConvertBuf) -> *const u8;
    fn mbs_buf_reset(buf: *mut ConvertBuf, len: usize);
    fn mbs_buf_append(buf: *mut ConvertBuf, bytes: *const u8, len: usize);
    fn mbs_string_free(str: *mut ZendString);
    fn free(ptr: *mut c_void);
    safe static mbfl_encoding_utf8: RawEncoding;
    safe static mbfl_encoding_pass: RawEncoding;
    safe static mbfl_encoding_8bit: RawEncoding;
    safe static mbfl_encoding_ascii: RawEncoding;
    safe static mbfl_encoding_base64: RawEncoding;
    safe static mbfl_encoding_qprint: RawEncoding;
    safe static mbfl_encoding_html_ent: RawEncoding;
    safe static mbfl_encoding_uuencode: RawEncoding;
    safe static mbfl_encoding_ucs4be: RawEncoding;
    safe static mbfl_language_neutral: RawLanguage;
}

/// A NUL-terminated C string of a static table, as bytes.
fn text(p: *const c_char) -> &'static [u8] {
    if p.is_null() {
        return b"";
    }
    // SAFETY: every name in libmbfl's tables is a NUL-terminated literal.
    unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes()
}

/// A NULL-terminated list of C strings in a static table.
fn list(mut p: *const *const c_char) -> Vec<&'static [u8]> {
    let mut out = Vec::new();
    // SAFETY: a table's alias list is a static array ending in NULL, and
    // `p` walks it no further than that NULL.
    while !p.is_null() && !unsafe { *p }.is_null() {
        // SAFETY: `p` points at a non-NULL entry of the list, as just read.
        out.push(text(unsafe { *p }));
        // SAFETY: the entry at `p` was not the terminator, so the next one
        // is still inside the list.
        p = unsafe { p.add(1) };
    }
    out
}

/// One of libmbfl's encodings, `const mbfl_encoding *`: a reference into its
/// static table, equal where php compares the pointers.
#[derive(Clone, Copy)]
pub(crate) struct Encoding(&'static RawEncoding);

// SAFETY: the table an `Encoding` points into is `static const` in
// libmbfl, immutable for the life of the process.
unsafe impl Send for Encoding {}
// SAFETY: as above; nothing reachable through it is ever written.
unsafe impl Sync for Encoding {}

impl PartialEq for Encoding {
    fn eq(&self, other: &Encoding) -> bool {
        ptr::eq(self.0, other.0)
    }
}

impl Eq for Encoding {}

impl std::fmt::Debug for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&String::from_utf8_lossy(self.name()))
    }
}

impl Encoding {
    fn of(p: *const RawEncoding) -> Option<Encoding> {
        // SAFETY: libmbfl answers NULL or a pointer into its static table.
        unsafe { p.as_ref() }.map(Encoding)
    }

    /// `mbfl_name2encoding_ex`: by name, MIME name or alias, ignoring
    /// case, over exactly the bytes given.
    pub(crate) fn named_ex(name: &[u8]) -> Option<Encoding> {
        // SAFETY: the function reads `name.len()` bytes of `name`.
        Encoding::of(unsafe { mbfl_name2encoding_ex(name.as_ptr().cast(), name.len()) })
    }

    /// `mbfl_name2encoding`: the name read as a C string, to its first
    /// NUL, as php passes `ZSTR_VAL`.
    pub(crate) fn named(name: &[u8]) -> Option<Encoding> {
        Encoding::named_ex(php_ext::coerce::c_str(name))
    }

    /// `mbfl_no2encoding`.
    pub(crate) fn numbered(no: i32) -> Option<Encoding> {
        Encoding::of(mbfl_no2encoding(no))
    }

    /// `mbfl_get_supported_encodings`, in its order.
    pub(crate) fn all() -> Vec<Encoding> {
        let mut out = Vec::new();
        let mut p = mbfl_get_supported_encodings();
        // SAFETY: the list is a static array of encodings ending in NULL.
        while let Some(encoding) = Encoding::of(unsafe { *p }) {
            out.push(encoding);
            // SAFETY: the entry just read was not the terminator.
            p = unsafe { p.add(1) };
        }
        out
    }

    pub(crate) fn utf8() -> Encoding {
        Encoding(&mbfl_encoding_utf8)
    }

    pub(crate) fn pass() -> Encoding {
        Encoding(&mbfl_encoding_pass)
    }

    pub(crate) fn eight_bit() -> Encoding {
        Encoding(&mbfl_encoding_8bit)
    }

    pub(crate) fn ascii() -> Encoding {
        Encoding(&mbfl_encoding_ascii)
    }

    pub(crate) fn base64() -> Encoding {
        Encoding(&mbfl_encoding_base64)
    }

    pub(crate) fn qprint() -> Encoding {
        Encoding(&mbfl_encoding_qprint)
    }

    pub(crate) fn html_ent() -> Encoding {
        Encoding(&mbfl_encoding_html_ent)
    }

    pub(crate) fn uuencode() -> Encoding {
        Encoding(&mbfl_encoding_uuencode)
    }

    pub(crate) fn ucs4be() -> Encoding {
        Encoding(&mbfl_encoding_ucs4be)
    }

    /// `no_encoding`, an `ENCODING_*` of `tables`.
    pub(crate) fn no(self) -> i32 {
        self.0.no_encoding
    }

    pub(crate) fn name(self) -> &'static [u8] {
        text(self.0.name)
    }

    /// `mime_name`, which may be empty.
    pub(crate) fn mime_name(self) -> &'static [u8] {
        text(self.0.mime_name)
    }

    pub(crate) fn aliases(self) -> Vec<&'static [u8]> {
        list(self.0.aliases)
    }

    pub(crate) fn flag(self) -> u32 {
        self.0.flag
    }

    pub(crate) fn mblen_table(self) -> Option<&'static [u8; 256]> {
        // SAFETY: a `mblen_table` is NULL or a static 256-byte table.
        unsafe { self.0.mblen_table.as_ref() }
    }

    pub(crate) fn has_check(self) -> bool {
        self.0.check.is_some()
    }

    /// `to_wchar`: codepoints decoded from the front of `input` into
    /// `out`, `input` advanced past what was read. A buffer shorter than
    /// `MBSTRING_MIN_WCHAR_BUFSIZE` decodes nothing, as no caller hands one.
    pub(crate) fn decode(self, input: &mut &[u8], out: &mut [u32], state: &mut u32) -> usize {
        let Some(to_wchar) = self.0.to_wchar else {
            return 0;
        };
        if out.len() < 5 || input.is_empty() {
            return 0;
        }
        let mut p = input.as_ptr().cast_mut();
        let mut len = input.len();
        // SAFETY: the decoder reads at most `len` bytes from `p` and writes
        // at most `out.len()` codepoints, never through `p`, and leaves `p`
        // and `len` describing the unread tail of `input`.
        let n = unsafe { to_wchar(&raw mut p, &raw mut len, out.as_mut_ptr(), out.len(), state) };
        *input = &input[input.len() - len..];
        n.min(out.len())
    }

    /// `check`, where the encoding has one.
    pub(crate) fn check(self, input: &[u8]) -> Option<bool> {
        let check = self.0.check?;
        // SAFETY: the checker reads `input.len()` bytes and writes none.
        Some(unsafe { check(input.as_ptr().cast_mut(), input.len()) })
    }

    /// `cut`, where the encoding has one: the bytes of whole characters
    /// from byte `from`, `len` bytes at most, as `mb_strcut` asks it.
    pub(crate) fn cut(self, input: &[u8], from: usize, len: usize) -> Option<Vec<u8>> {
        let cut = self.0.cut?;
        if from > input.len() {
            return Some(Vec::new());
        }
        let range = input.as_ptr_range();
        // SAFETY: the cutter reads inside `[start, end)` and answers a
        // string it allocated, or the interned empty one.
        let out = unsafe { cut(range.start.cast_mut(), from, len, range.end.cast_mut()) };
        Some(ZStr(out).to_vec())
    }

    /// `mbfl_encoding_preferred_mime_name`.
    pub(crate) fn preferred_mime_name(self) -> Option<&'static [u8]> {
        Some(self.mime_name()).filter(|name| !name.is_empty())
    }
}

/// A `zend_string` libmbfl made, freed on drop.
struct ZStr(*mut ZendString);

impl ZStr {
    fn to_vec(&self) -> Vec<u8> {
        // SAFETY: libmbfl answers a live string, its header as the shim
        // lays it out.
        let Some(s) = (unsafe { self.0.as_ref() }) else {
            return Vec::new();
        };
        // SAFETY: `val` holds `len` bytes, the string's own.
        unsafe { std::slice::from_raw_parts((&raw const s.val).cast::<u8>(), s.len) }.to_vec()
    }
}

impl Drop for ZStr {
    fn drop(&mut self) {
        // SAFETY: the string is libmbfl's, freed once, the empty one never.
        unsafe { mbs_string_free(self.0) }
    }
}

/// `mb_fast_convert`: the whole of `input` from one encoding to another,
/// with the number of errors met.
pub(crate) fn fast_convert(
    input: &[u8],
    from: Encoding,
    to: Encoding,
    replacement: u32,
    mode: u32,
) -> (Vec<u8>, u32) {
    let mut errors = 0;
    // SAFETY: the converter reads `input.len()` bytes of `input` and
    // answers a string it allocated.
    let out = ZStr(unsafe {
        mb_fast_convert(
            input.as_ptr().cast_mut(),
            input.len(),
            from.0,
            to.0,
            replacement,
            mode,
            &raw mut errors,
        )
    });
    (out.to_vec(), errors)
}

/// `mbfl_strcut`, for an encoding with neither `cut`, a fixed width nor
/// a `mblen_table`.
pub(crate) fn strcut(input: &[u8], encoding: Encoding, from: usize, len: usize) -> Vec<u8> {
    let mut string = MbflString {
        encoding: encoding.0,
        val: input.as_ptr().cast_mut(),
        len: input.len(),
    };
    let mut result = MbflString {
        encoding: encoding.0,
        val: ptr::null_mut(),
        len: 0,
    };
    // SAFETY: `string` describes `input`, which libmbfl only reads; the
    // result's bytes are allocated by it and freed below.
    unsafe { mbfl_strcut(&raw mut string, &raw mut result, from, len) };
    let out = if result.val.is_null() {
        Vec::new()
    } else {
        // SAFETY: the result holds `len` bytes at `val`.
        unsafe { std::slice::from_raw_parts(result.val, result.len) }.to_vec()
    };
    // SAFETY: the bytes were `emalloc`ed, `malloc` under the shim.
    unsafe { free(result.val.cast()) };
    out
}

/// `mb_convert_kana_codepoint` (`filters/mbfilter_cjk.c`): `c` converted
/// under `mode`, `consumed` set where `next` was folded into it, `second`
/// set to a codepoint that follows it.
pub(crate) fn kana(
    c: u32,
    next: u32,
    consumed: Option<&mut bool>,
    second: &mut u32,
    mode: u32,
) -> u32 {
    let consumed = consumed.map_or(ptr::null_mut(), ptr::from_mut);
    // SAFETY: the function writes through `consumed` where it is not NULL
    // and through `second`, both live for the call.
    unsafe { mb_convert_kana_codepoint(c, next, consumed, second, mode) }
}

/// One of libmbfl's languages, `const mbfl_language *`.
#[derive(Clone, Copy)]
pub(crate) struct Language(&'static RawLanguage);

// SAFETY: the table a `Language` points into is `static const`.
unsafe impl Send for Language {}
// SAFETY: as above.
unsafe impl Sync for Language {}

impl Language {
    fn of(p: *const RawLanguage) -> Option<Language> {
        // SAFETY: libmbfl answers NULL or a pointer into its static table.
        unsafe { p.as_ref() }.map(Language)
    }

    /// `mbfl_name2language`: the name, short name or an alias, ignoring
    /// case, the name read to its first NUL.
    pub(crate) fn named(name: &[u8]) -> Option<Language> {
        let name = CString::new(php_ext::coerce::c_str(name)).ok()?;
        // SAFETY: `name` is NUL-terminated and outlives the call.
        Language::of(unsafe { mbfl_name2language(name.as_ptr()) })
    }

    pub(crate) fn neutral() -> Language {
        Language(&mbfl_language_neutral)
    }

    pub(crate) fn no(self) -> i32 {
        self.0.no_language
    }

    pub(crate) fn name(self) -> &'static [u8] {
        text(self.0.name)
    }

    /// `mail_charset`, `mail_header_encoding` and `mail_body_encoding`.
    pub(crate) fn mail(self) -> [i32; 3] {
        [
            self.0.mail_charset,
            self.0.mail_header_encoding,
            self.0.mail_body_encoding,
        ]
    }
}

/// `mb_convert_buf`: text written in one encoding through its
/// `from_wchar`, owned and freed on drop.
pub(crate) struct Encoder {
    buf: ConvertBuf,
    encoding: Encoding,
}

impl Encoder {
    /// `mb_convert_buf_init`: room for `size` bytes, errors written as
    /// `mode` and `replacement` say (`MBFL_OUTPUTFILTER_ILLEGAL_MODE_*`).
    pub(crate) fn new(encoding: Encoding, size: usize, replacement: u32, mode: u32) -> Encoder {
        let buf = ConvertBuf {
            out: ptr::null_mut(),
            limit: ptr::null_mut(),
            state: 0,
            errors: 0,
            replacement_char: 0,
            error_mode: 0,
            str: ptr::null_mut(),
        };
        let mut encoder = Encoder { buf, encoding };
        // SAFETY: `buf` is a valid `mb_convert_buf` to initialise.
        unsafe { mbs_buf_init(&raw mut encoder.buf, size, replacement, mode) };
        encoder
    }

    /// `from_wchar`: `input` encoded onto the end, `end` flushing what a
    /// stateful encoding holds.
    pub(crate) fn write(&mut self, input: &[u32], end: bool) {
        let Some(from_wchar) = self.encoding.0.from_wchar else {
            return;
        };
        // SAFETY: the encoder reads `input.len()` codepoints, never writes
        // them, and grows the buffer it owns.
        unsafe {
            from_wchar(
                input.as_ptr().cast_mut(),
                input.len(),
                &raw mut self.buf,
                end,
            );
        }
    }

    pub(crate) fn len(&mut self) -> usize {
        // SAFETY: the buffer is initialised and owned.
        unsafe { mbs_buf_len(&raw mut self.buf) }
    }

    /// `mb_convert_buf_reset`: the bytes cut back to `len`, never grown.
    pub(crate) fn truncate(&mut self, len: usize) {
        let len = len.min(self.len());
        // SAFETY: `len` is at most what the buffer holds.
        unsafe { mbs_buf_reset(&raw mut self.buf, len) }
    }

    /// Bytes written as they are, past the encoding.
    pub(crate) fn append(&mut self, bytes: &[u8]) {
        // SAFETY: the buffer grows to take `bytes.len()` bytes read from
        // `bytes`.
        unsafe { mbs_buf_append(&raw mut self.buf, bytes.as_ptr(), bytes.len()) }
    }

    pub(crate) fn errors(&self) -> u32 {
        self.buf.errors
    }

    /// The encoding's shift state, which a caller backing up restores.
    pub(crate) fn state(&self) -> u32 {
        self.buf.state
    }

    pub(crate) fn set_state(&mut self, state: u32) {
        self.buf.state = state;
    }

    pub(crate) fn bytes(&mut self) -> &[u8] {
        let len = self.len();
        // SAFETY: the buffer is initialised and owned.
        let p = unsafe { mbs_buf_bytes(&raw mut self.buf) };
        // SAFETY: the buffer holds `len` written bytes from `p`, borrowed
        // no longer than `self`.
        unsafe { std::slice::from_raw_parts(p, len) }
    }

    pub(crate) fn into_vec(mut self) -> Vec<u8> {
        self.bytes().to_vec()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the buffer was initialised by `new` and is freed once.
        unsafe { mbs_buf_free(&raw mut self.buf) }
    }
}

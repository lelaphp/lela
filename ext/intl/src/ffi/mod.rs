//! ICU4C, the keg the oracle links (design §7.2, revision 75; #1573):
//! the C API php's intl calls, declared by hand under ICU's versioned
//! names, and php's own C++ over ICU's classes, reached through the
//! `extern "C"` shim in `shim/` that `build.rs` compiles. The one home of
//! this crate's `unsafe` (`unsafe.homes`' `ext/*/src/ffi/`); everything
//! above it is safe Rust over the owners and calls here.
//!
//! [`Code`] is `UErrorCode`, kept whole: php reports ICU's warnings
//! (`U_USING_DEFAULT_WARNING`) through `intl_get_error_code` as it does
//! its failures, so a call that can warn answers the code beside its
//! value, and one that cannot answers `Result<T, Code>`. Strings cross as
//! UTF-16 in a `Vec<u16>`, ICU's `UChar`, converted by [`to_utf16`] and
//! [`to_utf8`] as `intl_convert.c` converts them.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::{CStr, CString, c_char};
use std::ptr;

/// ICU's C functions under the names `urename.h` gives them, the major
/// version appended (`u_errorName_78`), which `build.rs` hands over as
/// `INTL_ICU_SUFFIX`.
macro_rules! icu {
    ($( $(#[$meta:meta])* fn $name:ident($($arg:ident: $ty:ty),* $(,)?) $(-> $ret:ty)?; )*) => {
        unsafe extern "C" { $(
            $(#[$meta])*
            #[link_name = concat!(stringify!($name), env!("INTL_ICU_SUFFIX"))]
            pub(crate) fn $name($($arg: $ty),*) $(-> $ret)?;
        )* }
    };
}
#[allow(unused_imports)]
pub(crate) use icu;

pub(crate) mod breakiterator;
pub(crate) mod calendar;
pub(crate) mod collator;
pub(crate) mod converter;
pub(crate) mod cpp;
pub(crate) mod dateformat;
pub(crate) mod enumeration;
pub(crate) mod formatter;
pub(crate) mod grapheme;
pub(crate) mod heap;
pub(crate) mod idn;
pub(crate) mod listformatter;
pub(crate) mod locale;
pub(crate) mod msgformat;
pub(crate) mod normalizer;
pub(crate) mod resourcebundle;
pub(crate) mod spoofchecker;
pub(crate) mod timezone;
pub(crate) mod transliterator;
pub(crate) mod uchar;

/// `UParseError`: where a rule or pattern parse stopped, and the UTF-16
/// around it, which `intl_parse_error_to_string` words.
#[repr(C)]
#[derive(Default)]
pub(crate) struct ParseError {
    pub(crate) line: i32,
    pub(crate) offset: i32,
    pub(crate) pre: [u16; 16],
    pub(crate) post: [u16; 16],
}

mod sys {
    use std::ffi::c_char;

    use super::Code;

    icu! {
        fn u_errorName(code: i32) -> *const c_char;
        fn u_strFromUTF8(
            dest: *mut u16,
            capacity: i32,
            length: *mut i32,
            src: *const c_char,
            src_length: i32,
            status: &mut Code,
        ) -> *mut u16;
        fn u_strToUTF8(
            dest: *mut c_char,
            capacity: i32,
            length: *mut i32,
            src: *const u16,
            src_length: i32,
            status: &mut Code,
        ) -> *mut c_char;
        fn uloc_getDefault() -> *const c_char;
        fn uloc_canonicalize(
            locale: *const c_char,
            name: *mut c_char,
            capacity: i32,
            status: &mut Code,
        ) -> i32;
        fn ucal_getTZDataVersion(status: &mut Code) -> *const c_char;
    }

    unsafe extern "C" {
        pub(crate) fn php_intl_icu_version() -> *const c_char;
        pub(crate) fn php_intl_icu_data_version() -> *const c_char;
        pub(crate) fn php_intl_unicode_version() -> *const c_char;
    }
}

/// `UErrorCode`: zero, a warning below zero or a failure above it, as
/// `U_FAILURE` reads it.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Code(pub(crate) i32);

impl Code {
    /// `U_ZERO_ERROR`.
    pub(crate) const ZERO: Code = Code(0);
    /// `U_ILLEGAL_ARGUMENT_ERROR`.
    pub(crate) const ILLEGAL_ARGUMENT: Code = Code(1);
    /// `U_INTERNAL_PROGRAM_ERROR`.
    pub(crate) const INTERNAL_PROGRAM: Code = Code(5);
    /// `U_MEMORY_ALLOCATION_ERROR`.
    pub(crate) const MEMORY_ALLOCATION: Code = Code(7);
    /// `U_INDEX_OUTOFBOUNDS_ERROR`.
    pub(crate) const INDEX_OUTOFBOUNDS: Code = Code(8);
    /// `U_INVALID_CHAR_FOUND`.
    pub(crate) const INVALID_CHAR_FOUND: Code = Code(10);
    /// `U_BUFFER_OVERFLOW_ERROR`.
    pub(crate) const BUFFER_OVERFLOW: Code = Code(15);
    /// `U_UNSUPPORTED_ERROR`.
    pub(crate) const UNSUPPORTED: Code = Code(16);
    /// `U_INVALID_STATE_ERROR`.
    pub(crate) const INVALID_STATE: Code = Code(27);
    /// `U_USING_FALLBACK_WARNING`.
    pub(crate) const USING_FALLBACK: Code = Code(-128);
    /// `U_USING_DEFAULT_WARNING`.
    pub(crate) const USING_DEFAULT: Code = Code(-127);
    /// `U_AMBIGUOUS_ALIAS_WARNING`.
    pub(crate) const AMBIGUOUS_ALIAS: Code = Code(-122);
    /// `U_STRING_NOT_TERMINATED_WARNING`.
    pub(crate) const STRING_NOT_TERMINATED: Code = Code(-124);

    /// `U_FAILURE`.
    pub(crate) fn is_failure(self) -> bool {
        self.0 > 0
    }

    /// What an ICU constructor answered beside this code, kept only where
    /// the code is no failure. ICU answers some objects half built beside
    /// a failure (`umsg_open` on `U_PATTERN_SYNTAX_ERROR`), and php stores
    /// that object before it checks the code and formats with it later,
    /// which reads what ICU never set (#2566). Here the object is closed by
    /// its owner's drop and never handed on, so a constructor finds none.
    pub(crate) fn built<T>(self, made: Option<T>) -> Option<T> {
        made.filter(|_| !self.is_failure())
    }

    /// The code as a `Result`: a failure is the error, zero or a warning
    /// the value.
    pub(crate) fn check(self) -> Result<Code, Code> {
        if self.is_failure() {
            Err(self)
        } else {
            Ok(self)
        }
    }

    /// `u_errorName`: the code's symbolic name, `[BOGUS UErrorCode]`
    /// outside ICU's tables.
    pub(crate) fn name(self) -> &'static str {
        // SAFETY: u_errorName takes any int32_t and answers a pointer to a
        // static, NUL-terminated name.
        let name = unsafe { sys::u_errorName(self.0) };
        static_str(name).unwrap_or("[BOGUS UErrorCode]")
    }
}

/// A NUL-terminated string ICU keeps for the process (`u_errorName`,
/// `uloc_getDefault`, a version), or `None` for a null pointer.
fn static_str(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: the pointer is non-null and ICU's: a NUL-terminated string
    // in static storage that outlives the process's use of it.
    let text = unsafe { CStr::from_ptr(ptr) };
    text.to_str().ok()
}

/// A Rust byte string as the `char *` php hands ICU: what precedes the
/// first NUL, as C reads it.
pub(crate) fn c_string(bytes: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
}

/// A length in ICU's `int32_t`, or `U_BUFFER_OVERFLOW_ERROR` where the
/// string is longer than ICU can count, as `intl_convert.c` refuses it.
pub(crate) fn length(len: usize) -> Result<i32, Code> {
    i32::try_from(len).map_err(|_| Code::BUFFER_OVERFLOW)
}

/// A capacity grown to fit `needed`, the preflight's answer.
fn capacity(needed: i32) -> usize {
    usize::try_from(needed).unwrap_or(0) + 1
}

/// ICU's preflight convention over a `UChar` buffer: `call` writes into
/// the buffer it is handed with its capacity and answers the full
/// length, overflowing with `U_BUFFER_OVERFLOW_ERROR` where it did not
/// fit, and is called again with room for it. The buffer is cut to the
/// length; the code is the last call's, a warning kept.
pub(crate) fn fill_utf16(
    mut call: impl FnMut(*mut u16, i32, &mut Code) -> i32,
) -> (Vec<u16>, Code) {
    let mut buffer = vec![0u16; 64];
    let mut status = Code::ZERO;
    let mut len = call(buffer.as_mut_ptr(), 63, &mut status);
    if status == Code::BUFFER_OVERFLOW || (status.0 <= 0 && len > 63) {
        buffer = vec![0u16; capacity(len)];
        status = Code::ZERO;
        let room = length(buffer.len() - 1).unwrap_or(i32::MAX);
        len = call(buffer.as_mut_ptr(), room, &mut status);
    }
    buffer.truncate(usize::try_from(len).unwrap_or(0).min(buffer.len()));
    (buffer, status)
}

/// [`fill_utf16`] over a `char` buffer.
pub(crate) fn fill_chars(
    mut call: impl FnMut(*mut c_char, i32, &mut Code) -> i32,
) -> (Vec<u8>, Code) {
    let mut buffer = vec![0u8; 160];
    let mut status = Code::ZERO;
    let mut len = call(buffer.as_mut_ptr().cast(), 159, &mut status);
    if status == Code::BUFFER_OVERFLOW || (status.0 <= 0 && len > 159) {
        buffer = vec![0u8; capacity(len)];
        status = Code::ZERO;
        let room = length(buffer.len() - 1).unwrap_or(i32::MAX);
        len = call(buffer.as_mut_ptr().cast(), room, &mut status);
    }
    buffer.truncate(usize::try_from(len).unwrap_or(0).min(buffer.len()));
    (buffer, status)
}

/// `intl_convert_utf8_to_utf16`: `u_strFromUTF8`, which refuses a
/// malformed sequence with `U_INVALID_CHAR_FOUND`.
pub(crate) fn to_utf16(text: &[u8]) -> Result<Vec<u16>, Code> {
    let len = length(text.len())?;
    let (out, status) = fill_utf16(|dest, capacity, status| {
        let mut written = 0;
        // SAFETY: dest holds capacity UChars and one more, the source
        // len bytes of a live slice; ICU writes no further than capacity.
        unsafe {
            sys::u_strFromUTF8(
                dest,
                capacity,
                &raw mut written,
                text.as_ptr().cast(),
                len,
                status,
            )
        };
        written
    });
    status.check().map(|_| out)
}

/// `intl_convert_utf8_to_utf16` as php makes it, into a block of the
/// request's heap: the length preflighted, then `eumalloc` of it and its
/// terminator, an ask the memory limit serves past the soft limit and
/// records, then the conversion. The block and the length of its text,
/// or the code a failure left, where nothing is held; the `Err` is the
/// hard limit.
pub(crate) fn to_utf16_block(
    req: &mut php_ext::request::Request,
    text: &[u8],
) -> Result<Result<(heap::Block<u16>, usize), Code>, php_ext::raising::Thrown> {
    let Ok(len) = length(text.len()) else {
        return Ok(Err(Code::BUFFER_OVERFLOW));
    };
    let (mut needed, mut status) = (0, Code::ZERO);
    // SAFETY: the preflight writes nothing, reading `len` bytes of a live
    // slice.
    unsafe {
        sys::u_strFromUTF8(
            std::ptr::null_mut(),
            0,
            &raw mut needed,
            text.as_ptr().cast(),
            len,
            &mut status,
        )
    };
    if status != Code::BUFFER_OVERFLOW && status != Code::STRING_NOT_TERMINATED {
        return Ok(Err(status));
    }
    let units = usize::try_from(needed).unwrap_or(0);
    let Some(mut block) = heap::Block::<u16>::ask(req, units + 1, false)? else {
        return Ok(Err(Code::MEMORY_ALLOCATION));
    };
    status = Code::ZERO;
    let room = block.as_mut_slice();
    // SAFETY: the block holds `needed + 1` units, the capacity handed, and
    // the source is `len` bytes of a live slice.
    unsafe {
        sys::u_strFromUTF8(
            room.as_mut_ptr(),
            needed + 1,
            std::ptr::null_mut(),
            text.as_ptr().cast(),
            len,
            &mut status,
        )
    };
    if status.is_failure() {
        block.give_back(req);
        return Ok(Err(status));
    }
    Ok(Ok((block, units)))
}

/// `intl_convert_utf16_to_utf8`: `u_strToUTF8`, which refuses an
/// unpaired surrogate with `U_INVALID_CHAR_FOUND`.
pub(crate) fn to_utf8(text: &[u16]) -> Result<Vec<u8>, Code> {
    let len = length(text.len())?;
    let (out, status) = fill_chars(|dest, capacity, status| {
        let mut written = 0;
        // SAFETY: dest holds capacity bytes and one more, the source len
        // UChars of a live slice; ICU writes no further than capacity.
        unsafe { sys::u_strToUTF8(dest, capacity, &raw mut written, text.as_ptr(), len, status) };
        written
    });
    status.check().map(|_| out)
}

/// `intl_convert_utf8_offset_to_utf16`: a byte offset into `text` as the
/// UTF-16 code units before it, by `u_strFromUTF8`'s preflight of the
/// bytes before it; an offset outside `0..=len` is kept as given, and one
/// inside a character refused with the code ICU answers,
/// `U_INVALID_CHAR_FOUND`.
pub(crate) fn utf8_offset_to_utf16(text: &[u8], position: i32) -> Result<i32, Code> {
    let Some(prefix) = usize::try_from(position).ok().and_then(|at| text.get(..at)) else {
        return Ok(position);
    };
    let mut units = 0;
    let mut status = Code::ZERO;
    // SAFETY: a preflight, which writes nothing, over position bytes of a
    // live slice.
    unsafe {
        sys::u_strFromUTF8(
            ptr::null_mut(),
            0,
            &raw mut units,
            prefix.as_ptr().cast(),
            position,
            &mut status,
        )
    };
    if status != Code::BUFFER_OVERFLOW && status.is_failure() {
        return Err(status);
    }
    Ok(units)
}

/// `intl_convert_utf16_offset_to_utf8`: a code unit offset into `text` as
/// the UTF-8 bytes before it, an offset between the two halves of a pair
/// moved back to the pair's start; one outside `0..=len`, or a prefix
/// `u_strToUTF8` refuses, is kept as code units.
pub(crate) fn utf16_offset_to_utf8(text: &[u16], position: i32) -> i32 {
    let Some(at) = usize::try_from(position)
        .ok()
        .filter(|&at| at <= text.len())
    else {
        return position;
    };
    let splits_pair = at > 0
        && at < text.len()
        && is_lead_surrogate(text[at - 1])
        && is_trail_surrogate(text[at]);
    let at = if splits_pair { at - 1 } else { at };
    let Ok(position) = i32::try_from(at) else {
        return position;
    };
    let mut bytes = 0;
    let mut status = Code::ZERO;
    // SAFETY: a preflight, which writes nothing, over at units of a live
    // slice.
    unsafe {
        sys::u_strToUTF8(
            ptr::null_mut(),
            0,
            &raw mut bytes,
            text[..at].as_ptr(),
            position,
            &mut status,
        )
    };
    if status != Code::BUFFER_OVERFLOW && status.is_failure() {
        return position;
    }
    bytes
}

/// `U16_IS_LEAD`.
fn is_lead_surrogate(unit: u16) -> bool {
    unit & 0xFC00 == 0xD800
}

/// `U16_IS_TRAIL`.
fn is_trail_surrogate(unit: u16) -> bool {
    unit & 0xFC00 == 0xDC00
}

/// `uloc_getDefault`: ICU's default locale for the process, which php
/// never sets (`intl_locale_get_default` reads `intl.default_locale`
/// first).
pub(crate) fn default_locale() -> &'static str {
    // SAFETY: uloc_getDefault takes nothing and answers ICU's static name.
    let name = unsafe { sys::uloc_getDefault() };
    static_str(name).unwrap_or("")
}

/// `uloc_canonicalize`, as `canonicalize_locale_string` calls it: into a
/// buffer of `ULOC_FULLNAME_CAPACITY`, `None` on a failure or an empty
/// answer.
pub(crate) fn canonicalize(locale: &[u8]) -> Option<Vec<u8>> {
    let locale = c_string(locale);
    let mut buffer = [0u8; FULLNAME_CAPACITY];
    let mut status = Code::ZERO;
    let room = length(buffer.len()).unwrap_or(0);
    // SAFETY: locale is NUL-terminated, the buffer holds room bytes.
    let len = unsafe {
        sys::uloc_canonicalize(
            locale.as_ptr(),
            buffer.as_mut_ptr().cast(),
            room,
            &mut status,
        )
    };
    let len = usize::try_from(len).ok().filter(|&n| n > 0)?;
    (!status.is_failure()).then(|| buffer[..len.min(buffer.len())].to_vec())
}

/// `ULOC_FULLNAME_CAPACITY`.
pub(crate) const FULLNAME_CAPACITY: usize = 157;

/// `U_ICU_VERSION`, ICU's compile-time version.
pub(crate) fn icu_version() -> &'static str {
    // SAFETY: the shim answers a string literal.
    static_str(unsafe { sys::php_intl_icu_version() }).unwrap_or("")
}

/// `U_ICU_DATA_VERSION`.
pub(crate) fn icu_data_version() -> &'static str {
    // SAFETY: the shim answers a string literal.
    static_str(unsafe { sys::php_intl_icu_data_version() }).unwrap_or("")
}

/// `U_UNICODE_VERSION`.
pub(crate) fn unicode_version() -> &'static str {
    // SAFETY: the shim answers a string literal.
    static_str(unsafe { sys::php_intl_unicode_version() }).unwrap_or("")
}

/// `ucal_getTZDataVersion`, `None` where ICU answers other than
/// `U_ZERO_ERROR`, as `PHP_MINFO_FUNCTION(intl)` leaves the row out.
pub(crate) fn tz_data_version() -> Option<&'static str> {
    let mut status = Code::ZERO;
    // SAFETY: the call takes a status and answers a static string.
    let version = unsafe { sys::ucal_getTZDataVersion(&mut status) };
    (status == Code::ZERO)
        .then(|| static_str(version))
        .flatten()
}

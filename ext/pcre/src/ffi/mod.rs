//! The binding of libpcre2-8 through `pcre2-sys` (design §7.2,
//! `unsafe.homes`' `ext/*/src/ffi/`): the calls `php_pcre.c` makes and
//! nothing beside them. Five owners, each freeing what it holds once:
//! [`Code`], a compiled pattern, immutable once `pcre2_jit_compile` has
//! seen it and
//! so shared by every request through the engine's cache; [`MatchData`],
//! the ovector a match writes; [`Context`], php's `mctx` with its limits
//! and its `pcre2_jit_stack`; and the `pcre2_compile_context` [`Code::compile`] makes
//! for a pattern that needs one; and [`Tables`], the character tables
//! `pcre2_maketables` builds under the thread's locale, shared as a
//! compiled pattern is. `MatchData` and `Context` are request-scoped,
//! php's `mdata` and `mctx`, which it frees at `RSHUTDOWN` while the
//! compiled pattern stays (design §2.3).
//!
//! A subject reaches the matcher through [`Subject`], which records
//! whether the library has already checked it as UTF-8: the one case in
//! which `PCRE2_NO_UTF_CHECK` is passed, or `pcre2_jit_match` called, over a
//! pattern that is UTF, so no call here hands the library bytes it would
//! misread.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::ffi::CStr;
use std::ptr::{self, NonNull};
use std::sync::Arc;

use pcre2_sys as sys;
use php_ext::values::PhpString;

pub(crate) use sys::{
    PCRE2_ANCHORED, PCRE2_CASELESS, PCRE2_DOLLAR_ENDONLY, PCRE2_DOTALL, PCRE2_DUPNAMES,
    PCRE2_ERROR_BACKSLASH_C_CALLER_DISABLED, PCRE2_ERROR_BADOFFSET, PCRE2_ERROR_BADUTFOFFSET,
    PCRE2_ERROR_JIT_STACKLIMIT, PCRE2_ERROR_MATCHLIMIT, PCRE2_ERROR_NOMATCH,
    PCRE2_ERROR_RECURSIONLIMIT, PCRE2_ERROR_UTF8_ERR1, PCRE2_ERROR_UTF8_ERR21, PCRE2_EXTENDED,
    PCRE2_EXTRA_CASELESS_RESTRICT, PCRE2_MULTILINE, PCRE2_NEVER_BACKSLASH_C, PCRE2_NO_AUTO_CAPTURE,
    PCRE2_NOTEMPTY_ATSTART, PCRE2_UCP, PCRE2_UNGREEDY, PCRE2_UNSET, PCRE2_UTF,
};

/// `PCRE_JIT_STACK_MIN_SIZE` and `PCRE_JIT_STACK_MAX_SIZE`.
const JIT_STACK_MIN: usize = 32 * 1024;
const JIT_STACK_MAX: usize = 192 * 1024;

/// A compiled pattern, `pcre2_code *`, freed with `pcre2_code_free`.
pub(crate) struct Code {
    raw: NonNull<sys::pcre2_code_8>,
    /// `PCRE2_UTF` among the options the pattern compiled with, set by
    /// the `u` modifier or by `(*UTF)` inside it.
    utf: bool,
    /// Whether `pcre2_jit_compile` left machine code behind.
    jitted: bool,
    /// The character tables the pattern compiled with, which it points
    /// into rather than copies, kept alive past `pcre2_code_free`.
    _tables: Option<Arc<Tables>>,
}

/// `pcre2_maketables(NULL)`: the character tables of the locale the
/// thread runs under, freed with `pcre2_maketables_free`.
pub(crate) struct Tables(NonNull<u8>);

// SAFETY: the tables are bytes the library wrote once at
// `pcre2_maketables` and only reads afterwards, from any thread.
unsafe impl Send for Tables {}
// SAFETY: as above; nothing writes them through `&Tables`.
unsafe impl Sync for Tables {}

impl Tables {
    /// The tables of the thread's `LC_CTYPE`, or `None` where the library
    /// could not allocate them.
    pub(crate) fn make() -> Option<Tables> {
        // SAFETY: a null general context asks for the system allocator.
        let raw = unsafe { sys::pcre2_maketables_8(ptr::null_mut()) };
        NonNull::new(raw.cast_mut()).map(Tables)
    }
}

impl Drop for Tables {
    fn drop(&mut self) {
        // SAFETY: made by `pcre2_maketables` with the default context and
        // freed here once, after every pattern that points into it.
        unsafe { sys::pcre2_maketables_free_8(ptr::null_mut(), self.0.as_ptr()) }
    }
}

// SAFETY: pcre2api(3), "Multithreading": a compiled pattern, JIT code
// included, may be shared by threads that match with it, since matching
// only reads it; the one write, `pcre2_jit_compile`, takes `&mut self` and
// so happens before the value is shared.
unsafe impl Send for Code {}
// SAFETY: as above; nothing reachable through `&Code` writes the pattern.
unsafe impl Sync for Code {}

impl Drop for Code {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `pcre2_compile` and is freed here once.
        unsafe { sys::pcre2_code_free_8(self.raw.as_ptr()) }
    }
}

/// What `pcre2_compile` refused a pattern with: the error number and
/// the offset it stopped at.
pub(crate) struct CompileError {
    pub(crate) number: i32,
    pub(crate) offset: usize,
}

/// A `pcre2_compile_context`, created only where a pattern needs an extra
/// option; every other compile passes the library's defaults.
struct CompileContext(NonNull<sys::pcre2_compile_context_8>);

impl Drop for CompileContext {
    fn drop(&mut self) {
        // SAFETY: created by `pcre2_compile_context_create`, freed once.
        unsafe { sys::pcre2_compile_context_free_8(self.0.as_ptr()) }
    }
}

impl Code {
    /// `pcre2_compile(pattern, len, options, ...)` with `extra` as the
    /// `pcre2_set_compile_extra_options` and `tables` as the
    /// `pcre2_set_character_tables`, the library's own where `None`.
    pub(crate) fn compile(
        pattern: &[u8],
        options: u32,
        extra: u32,
        tables: Option<Arc<Tables>>,
    ) -> Result<Code, CompileError> {
        let context = if extra == 0 && tables.is_none() {
            None
        } else {
            // SAFETY: a null general context asks for the system allocator.
            let raw = unsafe { sys::pcre2_compile_context_create_8(ptr::null_mut()) };
            let context = NonNull::new(raw).map(CompileContext);
            if let Some(context) = &context {
                // SAFETY: `context` is a live compile context.
                unsafe { sys::pcre2_set_compile_extra_options_8(context.0.as_ptr(), extra) };
                if let Some(tables) = &tables {
                    // SAFETY: `context` is live and the tables outlive the
                    // pattern, which keeps them.
                    unsafe {
                        sys::pcre2_set_character_tables_8(context.0.as_ptr(), tables.0.as_ptr())
                    };
                }
            }
            context
        };
        let context_ptr = context.as_ref().map_or(ptr::null_mut(), |c| c.0.as_ptr());
        let mut number = 0;
        let mut offset = 0;
        // SAFETY: the pointer and length describe `pattern`, which
        // outlives the call; the two out-parameters are live locals; the
        // context is live or null, which asks for the defaults.
        let raw = unsafe {
            sys::pcre2_compile_8(
                pattern.as_ptr(),
                pattern.len(),
                options,
                &raw mut number,
                &raw mut offset,
                context_ptr,
            )
        };
        let Some(raw) = NonNull::new(raw) else {
            return Err(CompileError { number, offset });
        };
        let mut all = 0u32;
        // SAFETY: `raw` is a live pattern and ALLOPTIONS writes a `u32`.
        unsafe {
            sys::pcre2_pattern_info_8(
                raw.as_ptr(),
                sys::PCRE2_INFO_ALLOPTIONS,
                (&raw mut all).cast(),
            )
        };
        Ok(Code {
            raw,
            utf: all & PCRE2_UTF != 0,
            jitted: false,
            _tables: tables,
        })
    }

    /// `pcre2_jit_compile(re, PCRE2_JIT_COMPLETE)`: `Ok` with whether
    /// machine code was left behind (`PCRE2_INFO_JITSIZE` above zero,
    /// php's `PREG_JIT`), or the library's negative code.
    pub(crate) fn jit(&mut self) -> Result<bool, i32> {
        // SAFETY: `raw` is a live pattern this value owns alone.
        let rc = unsafe { sys::pcre2_jit_compile_8(self.raw.as_ptr(), sys::PCRE2_JIT_COMPLETE) };
        if rc < 0 {
            return Err(rc);
        }
        let mut size = 0usize;
        // SAFETY: JITSIZE writes a `size_t` into the live local.
        let rc = unsafe {
            sys::pcre2_pattern_info_8(
                self.raw.as_ptr(),
                sys::PCRE2_INFO_JITSIZE,
                (&raw mut size).cast(),
            )
        };
        self.jitted = rc == 0 && size > 0;
        Ok(self.jitted)
    }

    /// One `u32` of `pcre2_pattern_info`.
    fn info(&self, what: u32) -> u32 {
        let mut value = 0u32;
        // SAFETY: CAPTURECOUNT and NAMECOUNT write a `uint32_t`.
        unsafe { sys::pcre2_pattern_info_8(self.raw.as_ptr(), what, (&raw mut value).cast()) };
        value
    }

    /// `PCRE2_INFO_CAPTURECOUNT`.
    pub(crate) fn capture_count(&self) -> u32 {
        self.info(sys::PCRE2_INFO_CAPTURECOUNT)
    }

    /// The name table, `make_subpats_table`'s walk: each entry's group
    /// number, its first two bytes big-endian, and its name, in the
    /// table's order.
    pub(crate) fn names(&self) -> Vec<(usize, Vec<u8>)> {
        let count = self.info(sys::PCRE2_INFO_NAMECOUNT) as usize;
        let size = self.info(sys::PCRE2_INFO_NAMEENTRYSIZE) as usize;
        if count == 0 || size < 3 {
            return Vec::new();
        }
        let mut table: *const u8 = ptr::null();
        // SAFETY: NAMETABLE writes a `PCRE2_SPTR` into the live local.
        let rc = unsafe {
            sys::pcre2_pattern_info_8(
                self.raw.as_ptr(),
                sys::PCRE2_INFO_NAMETABLE,
                (&raw mut table).cast(),
            )
        };
        if rc != 0 || table.is_null() {
            return Vec::new();
        }
        // SAFETY: the table is `count` entries of `size` bytes inside the
        // pattern, which `self` keeps alive while the slice is read.
        let bytes = unsafe { std::slice::from_raw_parts(table, count * size) };
        bytes
            .chunks_exact(size)
            .map(|entry| {
                let group = usize::from(entry[0]) << 8 | usize::from(entry[1]);
                let name = &entry[2..];
                (group, php_ext::coerce::c_str(name).to_vec())
            })
            .collect()
    }
}

/// `pcre2_match_data *`: room for the ovector of one match.
pub(crate) struct MatchData(NonNull<sys::pcre2_match_data_8>);

// SAFETY: the match data is owned by one request at a time and touched
// only by the thread serving it; nothing else holds the pointer.
unsafe impl Send for MatchData {}

impl Drop for MatchData {
    fn drop(&mut self) {
        // SAFETY: created by one of the two constructors, freed once.
        unsafe { sys::pcre2_match_data_free_8(self.0.as_ptr()) }
    }
}

impl MatchData {
    /// `pcre2_match_data_create(pairs, gctx)`.
    pub(crate) fn with_pairs(pairs: u32) -> Option<MatchData> {
        // SAFETY: a null general context asks for the system allocator.
        let raw = unsafe { sys::pcre2_match_data_create_8(pairs, ptr::null_mut()) };
        NonNull::new(raw).map(MatchData)
    }

    /// `pcre2_match_data_create_from_pattern(re, gctx)`.
    pub(crate) fn for_code(code: &Code) -> Option<MatchData> {
        // SAFETY: `code` is a live pattern; the context is the default.
        let raw = unsafe {
            sys::pcre2_match_data_create_from_pattern_8(code.raw.as_ptr(), ptr::null_mut())
        };
        NonNull::new(raw).map(MatchData)
    }

    /// The ovector, two offsets per pair, `PCRE2_UNSET` in each half of
    /// a group that did not take part.
    pub(crate) fn ovector(&self) -> &[usize] {
        // SAFETY: `self` is live match data.
        let pairs = unsafe { sys::pcre2_get_ovector_count_8(self.0.as_ptr()) } as usize;
        // SAFETY: as above; the pointer is to the data's own ovector.
        let at = unsafe { sys::pcre2_get_ovector_pointer_8(self.0.as_ptr()) };
        // SAFETY: the ovector is `pairs` pairs of `size_t`, owned by
        // `self`, which the returned borrow keeps alive and unwritten.
        unsafe { std::slice::from_raw_parts(at, pairs * 2) }
    }

    /// `pcre2_get_mark`: the name of the last `(*MARK)` the match passed.
    pub(crate) fn mark(&self) -> Option<Vec<u8>> {
        // SAFETY: `self` is live match data.
        let at = unsafe { sys::pcre2_get_mark_8(self.0.as_ptr()) };
        if at.is_null() {
            return None;
        }
        // SAFETY: a mark is a NUL-terminated name inside the pattern the
        // last match ran, which its caller still holds.
        let name = unsafe { CStr::from_ptr(at.cast()) };
        Some(name.to_bytes().to_vec())
    }
}

/// php's `mctx`, the `pcre2_match_context`, with the `pcre2_jit_stack` it assigns while
/// `pcre.jit` is on.
pub(crate) struct Context {
    raw: NonNull<sys::pcre2_match_context_8>,
    stack: Option<NonNull<sys::pcre2_jit_stack_8>>,
}

// SAFETY: the context and its stack are owned by one request at a time
// and touched only by the thread serving it.
unsafe impl Send for Context {}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: created by `pcre2_match_context_create`, freed once.
        unsafe { sys::pcre2_match_context_free_8(self.raw.as_ptr()) };
        if let Some(stack) = self.stack {
            // SAFETY: created by `pcre2_jit_stack_create`, freed once, after
            // the context that named it.
            unsafe { sys::pcre2_jit_stack_free_8(stack.as_ptr()) };
        }
    }
}

impl Context {
    /// `php_pcre_init_pcre2`'s `mctx` and `jit_stack`.
    pub(crate) fn new() -> Option<Context> {
        // SAFETY: a null general context asks for the system allocator.
        let raw = unsafe { sys::pcre2_match_context_create_8(ptr::null_mut()) };
        let raw = NonNull::new(raw)?;
        // SAFETY: as above; the sizes are php's.
        let stack =
            unsafe { sys::pcre2_jit_stack_create_8(JIT_STACK_MIN, JIT_STACK_MAX, ptr::null_mut()) };
        Some(Context {
            raw,
            stack: NonNull::new(stack),
        })
    }

    /// `OnUpdateBacktrackLimit`, `OnUpdateRecursionLimit` and
    /// `OnUpdateJit`: the two limits, and the `jit_stack` assigned or the
    /// library's own 32 KiB on the machine stack.
    pub(crate) fn configure(&mut self, match_limit: u32, depth_limit: u32, jit_stack: bool) {
        // SAFETY: `raw` is a live context this value owns.
        unsafe { sys::pcre2_set_match_limit_8(self.raw.as_ptr(), match_limit) };
        // SAFETY: as above.
        unsafe { sys::pcre2_set_depth_limit_8(self.raw.as_ptr(), depth_limit) };
        let stack = match self.stack {
            Some(stack) if jit_stack => stack.as_ptr().cast(),
            _ => ptr::null_mut(),
        };
        // SAFETY: a null callback with the stack as data assigns that
        // stack, which lives as long as the context; null data assigns none.
        unsafe { sys::pcre2_jit_stack_assign_8(self.raw.as_ptr(), None, stack) };
    }
}

/// A subject and whether it is known to be valid UTF-8: checked by the
/// library in an earlier call over it, or carrying `IS_STR_VALID_UTF8`,
/// which the engine sets only on a string whose every byte was checked.
pub(crate) struct Subject<'s> {
    bytes: &'s [u8],
    checked: bool,
}

impl<'s> Subject<'s> {
    /// A subject nothing is known of yet.
    pub(crate) fn new(bytes: &'s [u8]) -> Subject<'s> {
        Subject {
            bytes,
            checked: false,
        }
    }

    /// A string's bytes, known valid where the string says it was
    /// checked, php's `is_known_valid_utf8`.
    pub(crate) fn of(string: &'s PhpString) -> Subject<'s> {
        Subject {
            bytes: string.as_bytes(),
            checked: string.is_valid_utf8(),
        }
    }
}

/// What the library is handed for an empty subject: a live byte. An
/// empty slice's pointer dangles, and the vector search in a pattern's
/// machine code aligns the subject's pointer down to 16 bytes and loads
/// from there (`ffcs_mask`, `pcre2_jit_neon_inc.h`), which stays on the
/// subject's page for a live pointer and falls on page zero for a
/// dangling one; php's subject is a `zend_string`'s `val`, always live.
static EMPTY: u8 = 0;

/// One `pcre2_match` or `pcre2_jit_match` of `code` over `subject` from
/// `start`, with `options` among `PCRE2_NOTEMPTY_ATSTART` and
/// `PCRE2_ANCHORED`: the library's return, the pair count or a negative
/// code. `pcre2_jit_match` is called where `jit` allows it, the pattern has
/// machine code and the subject needs no check, which is when php takes
/// it; `PCRE2_NO_UTF_CHECK` is passed where the pattern is not UTF or the
/// subject was checked by an earlier call and `start` begins a character.
pub(super) fn run(
    code: &Code,
    subject: &mut Subject<'_>,
    start: usize,
    options: u32,
    data: &mut MatchData,
    context: &Context,
    jit: bool,
) -> i32 {
    let bytes = subject.bytes;
    if start > bytes.len() {
        return PCRE2_ERROR_BADOFFSET;
    }
    let trusted = !code.utf || subject.checked && bytes.get(start).is_none_or(|b| b & 0xc0 != 0x80);
    let options = if trusted {
        options | sys::PCRE2_NO_UTF_CHECK
    } else {
        options
    };
    let at = if bytes.is_empty() {
        &raw const EMPTY
    } else {
        bytes.as_ptr()
    };
    let rc = if jit && code.jitted && trusted {
        // SAFETY: the pattern has machine code; the subject is `bytes`,
        // alive for the call and at a live address even when empty, with
        // `start` within it and, under UTF, known valid (a check by the
        // library, or the string's own record of one) with `start` at a
        // character's start; data and `mctx` live.
        unsafe {
            sys::pcre2_jit_match_8(
                code.raw.as_ptr(),
                at,
                bytes.len(),
                start,
                options,
                data.0.as_ptr(),
                context.raw.as_ptr(),
            )
        }
    } else {
        // SAFETY: as above; the interpreter checks what the flags ask of it.
        unsafe {
            sys::pcre2_match_8(
                code.raw.as_ptr(),
                at,
                bytes.len(),
                start,
                options,
                data.0.as_ptr(),
                context.raw.as_ptr(),
            )
        }
    };
    if rc >= 0 || rc == PCRE2_ERROR_NOMATCH {
        subject.checked = true;
    }
    rc
}

/// `pcre2_get_error_message`, as php's 128-byte buffer holds it.
pub(super) fn error_message(number: i32) -> String {
    let mut buffer = [0u8; 128];
    // SAFETY: the buffer is live and its length is passed with it.
    let len = unsafe { sys::pcre2_get_error_message_8(number, buffer.as_mut_ptr(), buffer.len()) };
    let len = usize::try_from(len).unwrap_or(0).min(buffer.len());
    String::from_utf8_lossy(&buffer[..len]).into_owned()
}

/// `_pcre2_config_str`: one of the library's strings, the version, the
/// Unicode version or `PCRE2_CONFIG_JITTARGET`.
fn config_string(what: u32) -> Option<String> {
    let mut buffer = [0u8; 128];
    // SAFETY: every string item is shorter than 128 code units, and the
    // library writes at most the length it answers with a null pointer.
    let len = unsafe { sys::pcre2_config_8(what, buffer.as_mut_ptr().cast()) };
    let len = usize::try_from(len)
        .ok()?
        .saturating_sub(1)
        .min(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..len]).into_owned())
}

/// `PCRE2_CONFIG_VERSION`: `10.46 2025-08-27`.
pub(super) fn version() -> String {
    config_string(sys::PCRE2_CONFIG_VERSION).unwrap_or_default()
}

/// `PCRE2_CONFIG_UNICODE_VERSION`.
pub(super) fn unicode_version() -> String {
    config_string(sys::PCRE2_CONFIG_UNICODE_VERSION).unwrap_or_default()
}

/// `PCRE2_CONFIG_JITTARGET`.
pub(super) fn jit_target() -> Option<String> {
    config_string(sys::PCRE2_CONFIG_JITTARGET)
}

/// `PCRE2_CONFIG_JIT`: whether the library was built with `SUPPORT_JIT`.
pub(super) fn jit_supported() -> bool {
    let mut flag = 0u32;
    // SAFETY: CONFIG_JIT writes a `uint32_t` into the live local.
    let rc = unsafe { sys::pcre2_config_8(sys::PCRE2_CONFIG_JIT, (&raw mut flag).cast()) };
    rc == 0 && flag != 0
}

#[cfg(test)]
mod tests {
    use super::{Code, Context, MatchData, PCRE2_CASELESS, PCRE2_ERROR_NOMATCH, Subject, run};

    /// `/b/i`'s machine code searches forward for `b` or `B` with vector
    /// loads, which read around the subject's pointer; an empty slice's
    /// pointer dangles, where php's `val` is always live.
    #[test]
    fn an_empty_subject_whose_slice_dangles_is_matched_as_a_live_one() -> Result<(), &'static str> {
        let mut code = Code::compile(b"b", PCRE2_CASELESS, 0, None).map_err(|_| "`b` compiles")?;
        let jitted = code.jit().map_err(|_| "`b` compiles to machine code")?;
        let mut data = MatchData::with_pairs(1).ok_or("match data is allocated")?;
        let context = Context::new().ok_or("a match context is allocated")?;
        let empty: &[u8] = &[];
        let rc = run(
            &code,
            &mut Subject::new(empty),
            0,
            0,
            &mut data,
            &context,
            jitted,
        );
        assert_eq!(rc, PCRE2_ERROR_NOMATCH);
        Ok(())
    }
}

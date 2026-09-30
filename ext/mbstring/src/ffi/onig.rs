//! Oniguruma as `oniguruma.h` declares it, the calls `php_mbregex.c`
//! makes: a pattern compiled and searched with php's stack and retry
//! limits, the groups of a match, and the pattern's named groups.

use std::ffi::{c_int, c_uint, c_ulong, c_void};
use std::ptr::{self, NonNull};

#[repr(C)]
struct OnigRegion {
    allocated: c_int,
    num_regs: c_int,
    beg: *mut c_int,
    end: *mut c_int,
    history_root: *mut c_void,
}

#[repr(C)]
struct OnigErrorInfo {
    enc: *const c_void,
    par: *mut u8,
    par_end: *mut u8,
}

/// `OnigEncodingType` and `OnigSyntaxType`, only ever named by address.
#[repr(C)]
pub(crate) struct Opaque {
    _private: [u8; 0],
}

type NameFn = unsafe extern "C" fn(
    *const u8,
    *const u8,
    c_int,
    *mut c_int,
    *mut c_void,
    *mut c_void,
) -> c_int;

unsafe extern "C" {
    fn onig_initialize(encodings: *const *const Opaque, n: c_int) -> c_int;
    fn onig_new(
        reg: *mut *mut c_void,
        pattern: *const u8,
        pattern_end: *const u8,
        option: c_uint,
        enc: *const Opaque,
        syntax: *const Opaque,
        info: *mut OnigErrorInfo,
    ) -> c_int;
    fn onig_free(reg: *mut c_void);
    fn onig_error_code_to_str(s: *mut u8, code: c_int, ...) -> c_int;
    fn onig_region_new() -> *mut OnigRegion;
    fn onig_region_free(region: *mut OnigRegion, free_self: c_int);
    fn onig_new_match_param() -> *mut c_void;
    fn onig_initialize_match_param(mp: *mut c_void) -> c_int;
    fn onig_set_match_stack_limit_size_of_match_param(mp: *mut c_void, limit: c_uint) -> c_int;
    fn onig_set_retry_limit_in_match_of_match_param(mp: *mut c_void, limit: c_ulong) -> c_int;
    fn onig_free_match_param(mp: *mut c_void);
    fn onig_search_with_param(
        reg: *mut c_void,
        str: *const u8,
        end: *const u8,
        start: *const u8,
        range: *const u8,
        region: *mut OnigRegion,
        option: c_uint,
        mp: *mut c_void,
    ) -> c_int;
    fn onig_match_with_param(
        reg: *mut c_void,
        str: *const u8,
        end: *const u8,
        at: *const u8,
        region: *mut OnigRegion,
        option: c_uint,
        mp: *mut c_void,
    ) -> c_int;
    fn onig_number_of_names(reg: *mut c_void) -> c_int;
    fn onig_foreach_name(reg: *mut c_void, func: NameFn, arg: *mut c_void) -> c_int;
    fn onig_name_to_backref_number(
        reg: *mut c_void,
        name: *const u8,
        name_end: *const u8,
        region: *mut OnigRegion,
    ) -> c_int;
    fn onig_noname_group_capture_is_active(reg: *mut c_void) -> c_int;
    pub(crate) safe static OnigEncodingASCII: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_1: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_2: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_3: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_4: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_5: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_6: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_7: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_8: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_9: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_10: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_11: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_13: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_14: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_15: Opaque;
    pub(crate) safe static OnigEncodingISO_8859_16: Opaque;
    pub(crate) safe static OnigEncodingUTF8: Opaque;
    pub(crate) safe static OnigEncodingUTF16_BE: Opaque;
    pub(crate) safe static OnigEncodingUTF16_LE: Opaque;
    pub(crate) safe static OnigEncodingUTF32_BE: Opaque;
    pub(crate) safe static OnigEncodingUTF32_LE: Opaque;
    pub(crate) safe static OnigEncodingEUC_JP: Opaque;
    pub(crate) safe static OnigEncodingEUC_TW: Opaque;
    pub(crate) safe static OnigEncodingEUC_KR: Opaque;
    pub(crate) safe static OnigEncodingEUC_CN: Opaque;
    pub(crate) safe static OnigEncodingSJIS: Opaque;
    pub(crate) safe static OnigEncodingKOI8_R: Opaque;
    pub(crate) safe static OnigEncodingBIG5: Opaque;
    pub(crate) safe static OnigSyntaxJava: Opaque;
    pub(crate) safe static OnigSyntaxGnuRegex: Opaque;
    pub(crate) safe static OnigSyntaxGrep: Opaque;
    pub(crate) safe static OnigSyntaxEmacs: Opaque;
    pub(crate) safe static OnigSyntaxRuby: Opaque;
    pub(crate) safe static OnigSyntaxPerl: Opaque;
    pub(crate) safe static OnigSyntaxPosixBasic: Opaque;
    pub(crate) safe static OnigSyntaxPosixExtended: Opaque;
}

/// `ONIG_MISMATCH`.
pub(crate) const MISMATCH: i32 = -1;
/// `ONIG_MAX_ERROR_MESSAGE_LEN`.
const MAX_ERROR_MESSAGE_LEN: usize = 90;

/// An `OnigEncoding` or `OnigSyntaxType *`: a static table by address.
pub(crate) type Table = &'static Opaque;

/// `onig_init`, `PHP_MINIT_FUNCTION(mb_regex)`'s: once per process, the
/// library's own record keeping a second call a no-op.
pub(crate) fn init() {
    // SAFETY: no encoding is handed over; the library initialises itself.
    unsafe { onig_initialize(ptr::null(), 0) };
}

/// `onig_error_code_to_str` of a code that takes no parameter.
pub(crate) fn error_text(code: i32) -> String {
    let mut buf = [0u8; MAX_ERROR_MESSAGE_LEN];
    // SAFETY: the buffer is `ONIG_MAX_ERROR_MESSAGE_LEN` bytes, the length
    // the library writes at most.
    let n = unsafe { onig_error_code_to_str(buf.as_mut_ptr(), code) };
    String::from_utf8_lossy(&buf[..usize::try_from(n).unwrap_or(0).min(buf.len())]).into_owned()
}

/// A compiled pattern, `regex_t *`, freed on drop.
pub(crate) struct Regex(NonNull<c_void>);

// SAFETY: a compiled pattern is only read by searches once built, and a
// request holds its own; nothing is shared between threads.
unsafe impl Send for Regex {}
// SAFETY: as above, searching reads the pattern and writes the region.
unsafe impl Sync for Regex {}

impl Drop for Regex {
    fn drop(&mut self) {
        // SAFETY: the pattern is `onig_new`'s and is freed once.
        unsafe { onig_free(self.0.as_ptr()) }
    }
}

/// `OnigRegion`, freed on drop: the groups of the last match.
pub(crate) struct Region(NonNull<OnigRegion>);

// SAFETY: a region is the request's own, moved with it.
unsafe impl Send for Region {}

impl Region {
    pub(crate) fn new() -> Option<Region> {
        // SAFETY: allocates an empty region.
        NonNull::new(unsafe { onig_region_new() }).map(Region)
    }

    /// Each group's `(beg, end)`, `-1` for a group that took no part.
    pub(crate) fn groups(&self) -> Vec<(i32, i32)> {
        // SAFETY: the region is live and owned.
        let r = unsafe { self.0.as_ref() };
        let n = usize::try_from(r.num_regs).unwrap_or(0);
        if n == 0 || r.beg.is_null() || r.end.is_null() {
            return Vec::new();
        }
        // SAFETY: `beg` holds `num_regs` offsets.
        let beg = unsafe { std::slice::from_raw_parts(r.beg, n) };
        // SAFETY: `end` holds `num_regs` offsets.
        let end = unsafe { std::slice::from_raw_parts(r.end, n) };
        beg.iter().copied().zip(end.iter().copied()).collect()
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: the region is `onig_region_new`'s and is freed once.
        unsafe { onig_region_free(self.0.as_ptr(), 1) }
    }
}

/// `OnigMatchParam` with php's limits, freed on drop.
struct Param(*mut c_void);

impl Param {
    /// `_php_mb_onig_search`'s: each limit set where it fits an
    /// `unsigned int`.
    fn new(stack_limit: Option<u32>, retry_limit: Option<u32>) -> Param {
        // SAFETY: allocates a parameter block.
        let mp = unsafe { onig_new_match_param() };
        if !mp.is_null() {
            // SAFETY: `mp` is a fresh block.
            unsafe { onig_initialize_match_param(mp) };
            if let Some(limit) = stack_limit {
                // SAFETY: `mp` is initialised.
                unsafe { onig_set_match_stack_limit_size_of_match_param(mp, limit) };
            }
            if let Some(limit) = retry_limit {
                // SAFETY: `mp` is initialised.
                unsafe { onig_set_retry_limit_in_match_of_match_param(mp, c_ulong::from(limit)) };
            }
        }
        Param(mp)
    }
}

impl Drop for Param {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the block is `onig_new_match_param`'s, freed once.
            unsafe { onig_free_match_param(self.0) }
        }
    }
}

/// One named group as `onig_foreach_name` visits it, and its number as
/// `onig_name_to_backref_number` answers it for the match's groups.
unsafe extern "C" fn visit(
    name: *const u8,
    name_end: *const u8,
    _: c_int,
    _: *mut c_int,
    reg: *mut c_void,
    arg: *mut c_void,
) -> c_int {
    // SAFETY: `arg` is the `Visit` `names` handed the library.
    let visit = unsafe { &mut *arg.cast::<Visit>() };
    // SAFETY: the name lies in `[name, name_end)` of the pattern.
    let len = usize::try_from(unsafe { name_end.offset_from(name) }).unwrap_or(0);
    // SAFETY: as above.
    let text = unsafe { std::slice::from_raw_parts(name, len) }.to_vec();
    // SAFETY: the pattern and the region are live for the walk.
    let group = unsafe { onig_name_to_backref_number(reg, name, name_end, visit.region) };
    visit.names.push((text, group));
    0
}

struct Visit {
    region: *mut OnigRegion,
    names: Vec<(Vec<u8>, i32)>,
}

impl Regex {
    /// `onig_new`, its error as `onig_error_code_to_str` writes it.
    pub(crate) fn new(
        pattern: &[u8],
        options: u32,
        enc: Table,
        syntax: Table,
    ) -> Result<Regex, String> {
        let mut reg = ptr::null_mut();
        let mut info = OnigErrorInfo {
            enc: ptr::null(),
            par: ptr::null_mut(),
            par_end: ptr::null_mut(),
        };
        let range = pattern.as_ptr_range();
        // SAFETY: the library reads the pattern in `[start, end)` and
        // writes the new pattern or the error's parameters.
        let code = unsafe {
            onig_new(
                &raw mut reg,
                range.start,
                range.end,
                options,
                enc,
                syntax,
                &raw mut info,
            )
        };
        if code == 0
            && let Some(reg) = NonNull::new(reg)
        {
            return Ok(Regex(reg));
        }
        let mut buf = [0u8; MAX_ERROR_MESSAGE_LEN];
        // SAFETY: the buffer is the maximum length; `info` is what
        // `onig_new` filled for a code that takes it.
        let n = unsafe { onig_error_code_to_str(buf.as_mut_ptr(), code, &raw mut info) };
        Err(
            String::from_utf8_lossy(&buf[..usize::try_from(n).unwrap_or(0).min(buf.len())])
                .into_owned(),
        )
    }

    /// `onig_search_with_param` over `subject` from `start`: the match's
    /// offset, `ONIG_MISMATCH`, or an error code.
    pub(crate) fn search(
        &self,
        subject: &[u8],
        start: usize,
        region: &mut Region,
        limits: (Option<u32>, Option<u32>),
    ) -> i32 {
        // A start past the end is no match, as oniguruma answers it.
        let Some(from) = subject.get(start..) else {
            return MISMATCH;
        };
        let param = Param::new(limits.0, limits.1);
        let range = subject.as_ptr_range();
        let start = from.as_ptr();
        // SAFETY: the library reads `[str, end)` from `start` and writes the
        // owned region.
        unsafe {
            onig_search_with_param(
                self.0.as_ptr(),
                range.start,
                range.end,
                start,
                range.end,
                region.0.as_ptr(),
                0,
                param.0,
            )
        }
    }

    /// `onig_match_with_param` at the start of `subject`, no groups kept.
    pub(crate) fn matches_at_start(
        &self,
        subject: &[u8],
        limits: (Option<u32>, Option<u32>),
    ) -> i32 {
        let param = Param::new(limits.0, limits.1);
        let range = subject.as_ptr_range();
        // SAFETY: the library reads `[str, end)` and writes no region.
        unsafe {
            onig_match_with_param(
                self.0.as_ptr(),
                range.start,
                range.end,
                range.start,
                ptr::null_mut(),
                0,
                param.0,
            )
        }
    }

    /// The named groups, each with the group number the last match in
    /// `region` gives it, in the pattern's order.
    pub(crate) fn names(&self, region: &Region) -> Vec<(Vec<u8>, i32)> {
        // SAFETY: reads the pattern's name table.
        if unsafe { onig_number_of_names(self.0.as_ptr()) } <= 0 {
            return Vec::new();
        }
        let mut walk = Visit {
            region: region.0.as_ptr(),
            names: Vec::new(),
        };
        // SAFETY: `visit` is handed `walk`, live for the call.
        unsafe { onig_foreach_name(self.0.as_ptr(), visit, (&raw mut walk).cast()) };
        walk.names
    }

    /// `onig_name_to_backref_number`.
    pub(crate) fn backref(&self, name: &[u8], region: &Region) -> i32 {
        let range = name.as_ptr_range();
        // SAFETY: reads the name in `[start, end)`, the pattern and region.
        unsafe {
            onig_name_to_backref_number(self.0.as_ptr(), range.start, range.end, region.0.as_ptr())
        }
    }

    /// `onig_noname_group_capture_is_active`.
    pub(crate) fn numbered_groups(&self) -> bool {
        // SAFETY: reads the pattern.
        unsafe { onig_noname_group_capture_is_active(self.0.as_ptr()) != 0 }
    }
}

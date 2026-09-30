//! The declarations of `curl/curl.h`, `easy.h`, `multi.h` and
//! `options.h` the extension calls, written to the libcurl the oracle
//! links (8.21 at `oracle.toml`'s pin), which `build.rs` links: the
//! types by their C layout, each enumeration the C `int` it is, and the
//! functions php calls. What the library only points at is opaque.
#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_double, c_int, c_long, c_uint, c_void};

/// `CURL`.
#[repr(C)]
pub(crate) struct CURL {
    _opaque: [u8; 0],
}

/// `CURLM`.
#[repr(C)]
pub(crate) struct CURLM {
    _opaque: [u8; 0],
}

/// `CURLSH`.
#[repr(C)]
pub(crate) struct CURLSH {
    _opaque: [u8; 0],
}

/// `curl_mime`.
#[repr(C)]
pub(crate) struct CurlMime {
    _opaque: [u8; 0],
}

/// `curl_mimepart`.
#[repr(C)]
pub(crate) struct CurlMimePart {
    _opaque: [u8; 0],
}

/// `struct curl_pushheaders`.
#[repr(C)]
pub(crate) struct PushHeaders {
    _opaque: [u8; 0],
}

pub(crate) type CURLcode = c_int;
pub(crate) type CURLMcode = c_int;
pub(crate) type CURLSHcode = c_int;
pub(crate) type CURLoption = c_int;
pub(crate) type CURLINFO = c_int;
pub(crate) type curl_off_t = i64;

/// `struct curl_slist`.
#[repr(C)]
pub(crate) struct curl_slist {
    pub(crate) data: *mut c_char,
    pub(crate) next: *mut curl_slist,
}

/// `struct curl_blob`.
#[repr(C)]
pub(crate) struct curl_blob {
    pub(crate) data: *mut c_void,
    pub(crate) len: usize,
    pub(crate) flags: c_uint,
}

/// `struct curl_certinfo`.
#[repr(C)]
pub(crate) struct curl_certinfo {
    pub(crate) num_of_certs: c_int,
    pub(crate) certinfo: *mut *mut curl_slist,
}

/// `struct CURLMsg`: `data` is the union of `void *whatever` and
/// `CURLcode result`.
#[repr(C)]
pub(crate) struct CURLMsg {
    pub(crate) msg: c_int,
    pub(crate) easy_handle: *mut CURL,
    pub(crate) data: *mut c_void,
}

/// `curl_version_info_data` as far as `brotli_version`, the last field
/// php reads; the library's structure goes on past it and is only ever
/// read through the pointer it answers.
#[repr(C)]
pub(crate) struct curl_version_info_data {
    pub(crate) age: c_int,
    pub(crate) version: *const c_char,
    pub(crate) version_num: c_uint,
    pub(crate) host: *const c_char,
    pub(crate) features: c_int,
    pub(crate) ssl_version: *const c_char,
    pub(crate) ssl_version_num: c_long,
    pub(crate) libz_version: *const c_char,
    pub(crate) protocols: *const *const c_char,
    pub(crate) ares: *const c_char,
    pub(crate) ares_num: c_int,
    pub(crate) libidn: *const c_char,
    pub(crate) iconv_ver_num: c_int,
    pub(crate) libssh_version: *const c_char,
    pub(crate) brotli_ver_num: c_uint,
    pub(crate) brotli_version: *const c_char,
}

/// `curl_write_callback`, which `CURLOPT_HEADERFUNCTION` takes too.
pub(crate) type WriteFn = extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
/// `curl_read_callback`.
pub(crate) type ReadFn = extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize;
/// `curl_seek_callback`.
pub(crate) type SeekFn = extern "C" fn(*mut c_void, curl_off_t, c_int) -> c_int;
/// `curl_free_callback`.
pub(crate) type FreeFn = extern "C" fn(*mut c_void);
/// `curl_progress_callback`.
pub(crate) type ProgressFn =
    extern "C" fn(*mut c_void, c_double, c_double, c_double, c_double) -> c_int;
/// `curl_xferinfo_callback`.
pub(crate) type XferinfoFn =
    extern "C" fn(*mut c_void, curl_off_t, curl_off_t, curl_off_t, curl_off_t) -> c_int;
/// `curl_fnmatch_callback`.
pub(crate) type FnmatchFn = extern "C" fn(*mut c_void, *const c_char, *const c_char) -> c_int;
/// `curl_debug_callback`.
pub(crate) type DebugFn = extern "C" fn(*mut CURL, c_int, *mut c_char, usize, *mut c_void) -> c_int;
/// `curl_prereq_callback`.
pub(crate) type PrereqFn =
    extern "C" fn(*mut c_void, *mut c_char, *mut c_char, c_int, c_int) -> c_int;
/// `curl_sshhostkeycallback`.
pub(crate) type SshHostkeyFn = extern "C" fn(*mut c_void, c_int, *const c_char, usize) -> c_int;
/// `curl_push_callback`.
pub(crate) type PushFn =
    extern "C" fn(*mut CURL, *mut CURL, usize, *mut PushHeaders, *mut c_void) -> c_int;

unsafe extern "C" {
    pub(crate) fn curl_easy_init() -> *mut CURL;
    pub(crate) fn curl_easy_cleanup(curl: *mut CURL);
    pub(crate) fn curl_easy_duphandle(curl: *mut CURL) -> *mut CURL;
    pub(crate) fn curl_easy_reset(curl: *mut CURL);
    pub(crate) fn curl_easy_setopt(curl: *mut CURL, option: CURLoption, ...) -> CURLcode;
    pub(crate) fn curl_easy_getinfo(curl: *mut CURL, info: CURLINFO, ...) -> CURLcode;
    pub(crate) fn curl_easy_perform(curl: *mut CURL) -> CURLcode;
    pub(crate) fn curl_easy_pause(curl: *mut CURL, bitmask: c_int) -> CURLcode;
    pub(crate) fn curl_easy_upkeep(curl: *mut CURL) -> CURLcode;
    pub(crate) fn curl_easy_escape(
        curl: *mut CURL,
        string: *const c_char,
        length: c_int,
    ) -> *mut c_char;
    pub(crate) fn curl_easy_unescape(
        curl: *mut CURL,
        string: *const c_char,
        length: c_int,
        outlength: *mut c_int,
    ) -> *mut c_char;
    pub(crate) fn curl_easy_strerror(code: CURLcode) -> *const c_char;
    pub(crate) fn curl_free(ptr: *mut c_void);

    pub(crate) fn curl_multi_init() -> *mut CURLM;
    pub(crate) fn curl_multi_cleanup(multi: *mut CURLM) -> CURLMcode;
    pub(crate) fn curl_multi_add_handle(multi: *mut CURLM, easy: *mut CURL) -> CURLMcode;
    pub(crate) fn curl_multi_remove_handle(multi: *mut CURLM, easy: *mut CURL) -> CURLMcode;
    pub(crate) fn curl_multi_perform(multi: *mut CURLM, running: *mut c_int) -> CURLMcode;
    pub(crate) fn curl_multi_wait(
        multi: *mut CURLM,
        extra_fds: *mut c_void,
        extra_nfds: c_uint,
        timeout_ms: c_int,
        numfds: *mut c_int,
    ) -> CURLMcode;
    pub(crate) fn curl_multi_info_read(multi: *mut CURLM, queued: *mut c_int) -> *mut CURLMsg;
    pub(crate) fn curl_multi_setopt(multi: *mut CURLM, option: c_int, ...) -> CURLMcode;
    pub(crate) fn curl_multi_strerror(code: CURLMcode) -> *const c_char;
    pub(crate) fn curl_pushheader_bynum(headers: *mut PushHeaders, num: usize) -> *mut c_char;

    pub(crate) fn curl_share_init() -> *mut CURLSH;
    pub(crate) fn curl_share_cleanup(share: *mut CURLSH) -> CURLSHcode;
    pub(crate) fn curl_share_setopt(share: *mut CURLSH, option: c_int, ...) -> CURLSHcode;
    pub(crate) fn curl_share_strerror(code: CURLSHcode) -> *const c_char;

    pub(crate) fn curl_version_info(age: c_int) -> *mut curl_version_info_data;
    pub(crate) fn curl_slist_append(list: *mut curl_slist, line: *const c_char) -> *mut curl_slist;
    pub(crate) fn curl_slist_free_all(list: *mut curl_slist);

    pub(crate) fn curl_mime_init(easy: *mut CURL) -> *mut CurlMime;
    pub(crate) fn curl_mime_free(mime: *mut CurlMime);
    pub(crate) fn curl_mime_addpart(mime: *mut CurlMime) -> *mut CurlMimePart;
    pub(crate) fn curl_mime_name(part: *mut CurlMimePart, name: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_filename(part: *mut CurlMimePart, name: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_type(part: *mut CurlMimePart, mime: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_data(
        part: *mut CurlMimePart,
        data: *const c_char,
        size: usize,
    ) -> CURLcode;
    pub(crate) fn curl_mime_data_cb(
        part: *mut CurlMimePart,
        size: curl_off_t,
        read: Option<ReadFn>,
        seek: Option<SeekFn>,
        free: Option<FreeFn>,
        arg: *mut c_void,
    ) -> CURLcode;
}

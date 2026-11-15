//! libcurl's C surface as the extension calls it: `curl-sys`'s
//! declarations (design §7.2), and beside them the few of
//! `curl/curl.h`, `multi.h` and `options.h` that curl-sys 0.4 does not
//! declare, written in its shape to the libcurl the oracle links (8.21 at
//! `oracle.toml`'s pin): what the library only points at is an empty
//! enum, and each enumeration the C type curl-sys gives it.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int, c_void};

pub(crate) use curl_sys::*;

pub(crate) const CURLOPT_MIMEPOST: CURLoption = CURLOPTTYPE_OBJECTPOINT + 269;
pub(crate) const CURLOPT_PREREQDATA: CURLoption = CURLOPTTYPE_OBJECTPOINT + 313;
pub(crate) const CURLOPT_SSH_HOSTKEYDATA: CURLoption = CURLOPTTYPE_OBJECTPOINT + 317;
pub(crate) const CURLINFO_OFF_T: CURLINFO = 0x60_0000;

/// `curl_mime`.
pub(crate) enum curl_mime {}
/// `curl_mimepart`.
pub(crate) enum curl_mimepart {}
/// `struct curl_pushheaders`.
pub(crate) enum curl_pushheaders {}

pub(crate) type curl_xferinfo_callback =
    extern "C" fn(*mut c_void, curl_off_t, curl_off_t, curl_off_t, curl_off_t) -> c_int;
pub(crate) type curl_prereq_callback =
    extern "C" fn(*mut c_void, *mut c_char, *mut c_char, c_int, c_int) -> c_int;
pub(crate) type curl_sshhostkeycallback =
    extern "C" fn(*mut c_void, c_int, *const c_char, usize) -> c_int;
pub(crate) type curl_push_callback =
    extern "C" fn(*mut CURL, *mut CURL, usize, *mut curl_pushheaders, *mut c_void) -> c_int;

unsafe extern "C" {
    pub(crate) fn curl_pushheader_bynum(h: *mut curl_pushheaders, num: usize) -> *mut c_char;
    pub(crate) fn curl_mime_init(easy: *mut CURL) -> *mut curl_mime;
    pub(crate) fn curl_mime_free(mime: *mut curl_mime);
    pub(crate) fn curl_mime_addpart(mime: *mut curl_mime) -> *mut curl_mimepart;
    pub(crate) fn curl_mime_name(part: *mut curl_mimepart, name: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_filename(part: *mut curl_mimepart, name: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_type(part: *mut curl_mimepart, mime: *const c_char) -> CURLcode;
    pub(crate) fn curl_mime_data(
        part: *mut curl_mimepart,
        data: *const c_char,
        size: usize,
    ) -> CURLcode;
    pub(crate) fn curl_mime_data_cb(
        part: *mut curl_mimepart,
        size: curl_off_t,
        read: Option<curl_read_callback>,
        seek: Option<curl_seek_callback>,
        free: Option<curl_free_callback>,
        arg: *mut c_void,
    ) -> CURLcode;
}

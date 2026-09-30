//! uriparser's ANSI interface as the vendored headers declare it
//! (uriparser 1.0.2, php-src's copy): `UriUriA` and the structs it names
//! laid out as C lays them out, and the sixteen functions
//! `uri_parser_rfc3986.c` calls, with the memory manager php hands each
//! of them.
#![allow(non_camel_case_types, non_snake_case, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

pub(crate) type UriBool = c_int;

pub(crate) const URI_SUCCESS: c_int = 0;
pub(crate) const URI_ERROR_SYNTAX: c_int = 1;
pub(crate) const URI_ERROR_MALLOC: c_int = 3;
pub(crate) const URI_ERROR_ADDBASE_REL_BASE: c_int = 5;
pub(crate) const URI_ERROR_SETUSERINFO_HOST_NOT_SET: c_int = 12;
pub(crate) const URI_ERROR_SETPORT_HOST_NOT_SET: c_int = 13;
pub(crate) const URI_ERROR_SETHOST_USERINFO_SET: c_int = 14;
pub(crate) const URI_ERROR_SETHOST_PORT_SET: c_int = 15;

/// `UriResolutionOptions`' `URI_RESOLVE_STRICTLY`.
pub(crate) const URI_RESOLVE_STRICTLY: c_uint = 0;

/// `UriTextRangeA`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct UriTextRangeA {
    pub(crate) first: *const c_char,
    pub(crate) afterLast: *const c_char,
}

/// `UriPathSegmentA`.
#[repr(C)]
pub(crate) struct UriPathSegmentA {
    pub(crate) text: UriTextRangeA,
    pub(crate) next: *mut UriPathSegmentA,
    pub(crate) reserved: *mut c_void,
}

/// `UriHostDataA`.
#[repr(C)]
pub(crate) struct UriHostDataA {
    pub ip4: *mut c_void,
    pub ip6: *mut c_void,
    pub(crate) ipFuture: UriTextRangeA,
}

/// `UriMemoryManager` (`UriBase.h`): the five allocator functions
/// uriparser calls with the manager itself, and the data they reach it
/// through.
#[repr(C)]
pub(crate) struct UriMemoryManager {
    pub(crate) malloc: unsafe extern "C" fn(*mut UriMemoryManager, usize) -> *mut c_void,
    pub(crate) calloc: unsafe extern "C" fn(*mut UriMemoryManager, usize, usize) -> *mut c_void,
    pub(crate) realloc:
        unsafe extern "C" fn(*mut UriMemoryManager, *mut c_void, usize) -> *mut c_void,
    pub(crate) reallocarray:
        unsafe extern "C" fn(*mut UriMemoryManager, *mut c_void, usize, usize) -> *mut c_void,
    pub(crate) free: unsafe extern "C" fn(*mut UriMemoryManager, *mut c_void),
    pub(crate) userData: *mut c_void,
}

/// `UriUriA`.
#[repr(C)]
pub(crate) struct UriUriA {
    pub(crate) scheme: UriTextRangeA,
    pub(crate) userInfo: UriTextRangeA,
    pub(crate) hostText: UriTextRangeA,
    pub(crate) hostData: UriHostDataA,
    pub(crate) portText: UriTextRangeA,
    pub(crate) pathHead: *mut UriPathSegmentA,
    pub(crate) pathTail: *mut UriPathSegmentA,
    pub(crate) query: UriTextRangeA,
    pub(crate) fragment: UriTextRangeA,
    pub(crate) absolutePath: UriBool,
    pub(crate) owner: UriBool,
    pub(crate) reserved: *mut c_void,
}

const NO_RANGE: UriTextRangeA = UriTextRangeA {
    first: std::ptr::null(),
    afterLast: std::ptr::null(),
};

impl UriUriA {
    /// `UriUriA uri = {0};`.
    pub(crate) fn empty() -> UriUriA {
        UriUriA {
            scheme: NO_RANGE,
            userInfo: NO_RANGE,
            hostText: NO_RANGE,
            hostData: UriHostDataA {
                ip4: std::ptr::null_mut(),
                ip6: std::ptr::null_mut(),
                ipFuture: NO_RANGE,
            },
            portText: NO_RANGE,
            pathHead: std::ptr::null_mut(),
            pathTail: std::ptr::null_mut(),
            query: NO_RANGE,
            fragment: NO_RANGE,
            absolutePath: 0,
            owner: 0,
            reserved: std::ptr::null_mut(),
        }
    }
}

unsafe extern "C" {
    pub(crate) fn uriParseSingleUriExMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        errorPos: *mut *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriAddBaseUriExMmA(
        absoluteDest: *mut UriUriA,
        relativeSource: *const UriUriA,
        absoluteBase: *const UriUriA,
        options: c_uint,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriFreeUriMembersMmA(uri: *mut UriUriA, memory: *mut c_void) -> c_int;
    pub(crate) fn uriMakeOwnerMmA(uri: *mut UriUriA, memory: *mut c_void) -> c_int;
    pub(crate) fn uriCopyUriMmA(
        destUri: *mut UriUriA,
        sourceUri: *const UriUriA,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriNormalizeSyntaxExMmA(
        uri: *mut UriUriA,
        mask: c_uint,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriToStringCharsRequiredA(
        uri: *const UriUriA,
        charsRequired: *mut c_int,
    ) -> c_int;
    pub(crate) fn uriToStringA(
        dest: *mut c_char,
        uri: *const UriUriA,
        maxChars: c_int,
        charsWritten: *mut c_int,
    ) -> c_int;
    pub(crate) fn uriHasHostA(uri: *const UriUriA) -> UriBool;
    pub(crate) fn uriSetSchemeMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetUserInfoMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetHostAutoMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetPortTextMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetPathMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetQueryMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
    pub(crate) fn uriSetFragmentMmA(
        uri: *mut UriUriA,
        first: *const c_char,
        afterLast: *const c_char,
        memory: *mut c_void,
    ) -> c_int;
}

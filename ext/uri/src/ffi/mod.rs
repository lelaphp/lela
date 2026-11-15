//! The binding to the vendored uriparser (design §7.2;
//! `unsafe.homes`' `ext/*/src/ffi/`): the one home of this crate's
//! `unsafe`, over what `build.rs` compiles. [`Uri`] is one `UriUriA` that
//! owns its text, with the calls `uri_parser_rfc3986.c` makes on it: the
//! parse and the resolution against a base, the copy, the syntax
//! normalisation, the recomposition and the seven setters, each answer
//! decoded once into [`Error`].
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod sys;

use std::ffi::{c_int, c_void};
use std::ptr;

use php_ext::raising::Thrown;
use php_ext::request::{Request, ecalloc, efree, emalloc, erealloc};

/// uriparser's `URI_ERROR_*` other than `URI_SUCCESS`, the ones
/// `uri_parser_rfc3986.c` tells apart named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    /// `URI_ERROR_SYNTAX`.
    Syntax,
    /// `URI_ERROR_ADDBASE_REL_BASE`: the base is not absolute.
    RelativeBase,
    /// `URI_ERROR_SETUSERINFO_HOST_NOT_SET`.
    UserInfoWithoutHost,
    /// `URI_ERROR_SETPORT_HOST_NOT_SET`.
    PortWithoutHost,
    /// `URI_ERROR_SETHOST_USERINFO_SET`.
    HostRemovedUnderUserInfo,
    /// `URI_ERROR_SETHOST_PORT_SET`.
    HostRemovedUnderPort,
    /// Any other code, `URI_ERROR_MALLOC` among them, which php calls
    /// unreachable in practice.
    Other,
}

impl Error {
    fn check(code: c_int) -> Result<(), Error> {
        Err(match code {
            sys::URI_SUCCESS => return Ok(()),
            sys::URI_ERROR_SYNTAX => Error::Syntax,
            sys::URI_ERROR_ADDBASE_REL_BASE => Error::RelativeBase,
            sys::URI_ERROR_SETUSERINFO_HOST_NOT_SET => Error::UserInfoWithoutHost,
            sys::URI_ERROR_SETPORT_HOST_NOT_SET => Error::PortWithoutHost,
            sys::URI_ERROR_SETHOST_USERINFO_SET => Error::HostRemovedUnderUserInfo,
            sys::URI_ERROR_SETHOST_PORT_SET => Error::HostRemovedUnderPort,
            _ => Error::Other,
        })
    }
}

/// Where [`Uri::parse`] stopped: the parse of the text, or its resolution
/// against the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Parse(Error),
    Resolve(Error),
}

/// `php_uri_parser_rfc3986_memory_manager_malloc`
/// (`Z/ext/uri/uri_parser_rfc3986.c:31-34`): `emalloc`, the bytes left
/// as the heap the call is lent finds them.
unsafe extern "C" fn mm_malloc(_manager: *mut sys::UriMemoryManager, size: usize) -> *mut c_void {
    emalloc(size)
}

/// `php_uri_parser_rfc3986_memory_manager_calloc` (`:36-39`): `ecalloc`,
/// the one of the five that zeroes.
unsafe extern "C" fn mm_calloc(
    _manager: *mut sys::UriMemoryManager,
    nmemb: usize,
    size: usize,
) -> *mut c_void {
    ecalloc(nmemb, size)
}

/// `php_uri_parser_rfc3986_memory_manager_realloc` (`:41-44`):
/// `erealloc`, the bytes past the old length left as they are found.
unsafe extern "C" fn mm_realloc(
    _manager: *mut sys::UriMemoryManager,
    at: *mut c_void,
    size: usize,
) -> *mut c_void {
    // SAFETY: uriparser resizes only a block the manager answered, under
    // the lend of the call it is in.
    unsafe { erealloc(at, size) }
}

/// `php_uri_parser_rfc3986_memory_manager_reallocarray` (`:46-49`):
/// `safe_erealloc(ptr, nmemb, size, 0)`; a product past the address
/// space is refused with null.
unsafe extern "C" fn mm_reallocarray(
    _manager: *mut sys::UriMemoryManager,
    at: *mut c_void,
    nmemb: usize,
    size: usize,
) -> *mut c_void {
    let Some(len) = nmemb.checked_mul(size) else {
        return ptr::null_mut();
    };
    // SAFETY: as in `mm_realloc`.
    unsafe { erealloc(at, len) }
}

/// `php_uri_parser_rfc3986_memory_manager_destroy` (`:51-54`): `efree`.
unsafe extern "C" fn mm_free(_manager: *mut sys::UriMemoryManager, at: *mut c_void) {
    // SAFETY: uriparser frees a block once, and only one the manager
    // answered, under the lend of the call it is in.
    unsafe { efree(at) }
}

/// One `UriUriA` owning its text (`owner` set), boxed so the struct the
/// library filled never moves, with every allocation uriparser made for
/// it on the request's heap (design §5.13). Every call that allocates
/// is made through [`Uri::call`], which lends the manager the request
/// for that call alone. [`Uri::free`] gives the members back; a URI
/// dropped without is the request's end's, and uriparser is not called
/// from `Drop`, since the members are in memory the heap may already
/// have given back.
pub(crate) struct Uri {
    uri: Box<sys::UriUriA>,
}

// SAFETY: the URI and every allocation it points at are owned by this
// value alone and reached only through it.
unsafe impl Send for Uri {}

impl Uri {
    /// `UriUriA uri = {0};`.
    fn empty() -> Uri {
        Uri {
            uri: Box::new(sys::UriUriA::empty()),
        }
    }

    /// `call` over the URI with php's memory manager and `req`'s heap
    /// lent for its length (ADR-0014), so what uriparser allocates and
    /// frees is the request's; the `Err` is the memory limit's fatal an
    /// allocation met. The lend is made inside a fallible scope, as php
    /// 8.5.12's manager asks with `femalloc`, `fecalloc` and
    /// `safe_ferealloc` (`Z/ext/uri/uri_parser_rfc3986.c:31-50`): past
    /// the soft limit an allocation answers null, which uriparser answers
    /// with `URI_ERROR_MALLOC`, and the native's error is given up for the
    /// limit's fatal at the check after the call.
    fn call(
        &mut self,
        req: &mut Request,
        call: impl FnOnce(*mut sys::UriUriA, *mut c_void) -> c_int,
    ) -> Result<c_int, Thrown> {
        let mut manager = sys::UriMemoryManager {
            malloc: mm_malloc,
            calloc: mm_calloc,
            realloc: mm_realloc,
            reallocarray: mm_reallocarray,
            free: mm_free,
            userData: ptr::null_mut(),
        };
        let uri = &raw mut *self.uri;
        let manager = (&raw mut manager).cast();
        let lent = req
            .memory_mut()
            .fallible(|req| req.memory_mut().lend_heap(|| call(uri, manager)))?;
        Ok(lent.unwrap_or(sys::URI_ERROR_MALLOC))
    }

    /// `uriFreeUriMembersMmA`: the members given back to the request's
    /// heap.
    pub(crate) fn free(mut self, req: &mut Request) {
        // SAFETY: a URI zeroed or filled by the library, not reached after.
        self.call(req, |uri, memory| unsafe {
            sys::uriFreeUriMembersMmA(uri, memory)
        })
        .ok();
    }

    /// `php_uri_parser_rfc3986_parse_ex` up to its range check:
    /// `uriParseSingleUriExMmA` over `text`, resolved against `base` with
    /// `uriAddBaseUriExMmA` with `URI_RESOLVE_STRICTLY` where there is one, and made
    /// the owner of its text with `uriMakeOwnerMmA`; the outer `Err` is
    /// the memory limit's fatal.
    pub(crate) fn parse(
        req: &mut Request,
        text: &[u8],
        base: Option<&Uri>,
    ) -> Result<Result<Uri, Refusal>, Thrown> {
        let mut uri = Uri::empty();
        let range = text.as_ptr_range();
        // SAFETY: a zeroed URI, and the live bytes of `text`, which the
        // URI points into until it owns its text below.
        let code = uri.call(req, |uri, memory| unsafe {
            sys::uriParseSingleUriExMmA(
                uri,
                range.start.cast(),
                range.end.cast(),
                ptr::null_mut(),
                memory,
            )
        });
        if let Some(refused) = Uri::failed(req, &mut uri, code, Refusal::Parse)? {
            return Ok(Err(refused));
        }
        if let Some(base) = base {
            let mut resolved = Uri::empty();
            let source: *const sys::UriUriA = &raw const *uri.uri;
            let against: *const sys::UriUriA = &raw const *base.uri;
            // SAFETY: a zeroed destination, the parsed URI and a live
            // base, three distinct structs, as the library requires.
            let code = resolved.call(req, |dest, memory| unsafe {
                sys::uriAddBaseUriExMmA(dest, source, against, sys::URI_RESOLVE_STRICTLY, memory)
            });
            uri.free(req);
            if let Some(refused) = Uri::failed(req, &mut resolved, code, Refusal::Resolve)? {
                return Ok(Err(refused));
            }
            uri = resolved;
        }
        // SAFETY: a URI pointing into `text` and the base, both live; it
        // owns copies of its text from here.
        let code = uri.call(req, |uri, memory| unsafe {
            sys::uriMakeOwnerMmA(uri, memory)
        });
        if let Some(refused) = Uri::failed(req, &mut uri, code, Refusal::Parse)? {
            return Ok(Err(refused));
        }
        Ok(Ok(uri))
    }

    /// A call's refusal, with the URI it was filling freed: the fatal as
    /// the `Err`, uriparser's error as the `Some`.
    fn failed(
        req: &mut Request,
        uri: &mut Uri,
        code: Result<c_int, Thrown>,
        as_refusal: fn(Error) -> Refusal,
    ) -> Result<Option<Refusal>, Thrown> {
        let refusal = match code {
            Ok(code) => match Error::check(code) {
                Ok(()) => return Ok(None),
                Err(error) => Ok(Some(as_refusal(error))),
            },
            Err(thrown) => Err(thrown),
        };
        std::mem::replace(uri, Uri::empty()).free(req);
        refusal
    }

    /// `copy_uri`: `uriCopyUriMmA` into a fresh URI; the outer `Err` is
    /// the memory limit's fatal.
    pub(crate) fn try_clone(&self, req: &mut Request) -> Result<Result<Uri, Error>, Thrown> {
        let mut copy = Uri::empty();
        let source: *const sys::UriUriA = &raw const *self.uri;
        // SAFETY: a zeroed destination and a live source.
        let code = copy.call(req, |dest, memory| unsafe {
            sys::uriCopyUriMmA(dest, source, memory)
        });
        match code.map(Error::check) {
            Ok(Ok(())) => Ok(Ok(copy)),
            Ok(Err(error)) => {
                copy.free(req);
                Ok(Err(error))
            }
            Err(thrown) => {
                copy.free(req);
                Err(thrown)
            }
        }
    }

    /// `get_normalized_uri`: a copy with every part normalised,
    /// `uriNormalizeSyntaxExMmA` under the mask `(unsigned int)-1`; the
    /// outer `Err` is the memory limit's fatal.
    pub(crate) fn normalized(&self, req: &mut Request) -> Result<Result<Uri, Error>, Thrown> {
        let mut copy = match self.try_clone(req)? {
            Ok(copy) => copy,
            Err(error) => return Ok(Err(error)),
        };
        // SAFETY: a URI this owns.
        let code = copy.call(req, |uri, memory| unsafe {
            sys::uriNormalizeSyntaxExMmA(uri, u32::MAX, memory)
        });
        match code.map(Error::check) {
            Ok(Ok(())) => Ok(Ok(copy)),
            Ok(Err(error)) => {
                copy.free(req);
                Ok(Err(error))
            }
            Err(thrown) => {
                copy.free(req);
                Err(thrown)
            }
        }
    }

    /// `scheme`, `None` without a text range.
    pub(crate) fn scheme(&self) -> Option<&[u8]> {
        range(&self.uri.scheme)
    }

    /// `userInfo`.
    pub(crate) fn user_info(&self) -> Option<&[u8]> {
        range(&self.uri.userInfo)
    }

    /// `hostText`, which excludes the brackets of an IP address.
    pub(crate) fn host_text(&self) -> Option<&[u8]> {
        range(&self.uri.hostText)
    }

    /// Whether the host is an IPv6 or an `IPvFuture` address, which php
    /// shows in its brackets.
    pub(crate) fn host_is_ip_literal(&self) -> bool {
        !self.uri.hostData.ip6.is_null() || !self.uri.hostData.ipFuture.first.is_null()
    }

    /// `portText`.
    pub(crate) fn port_text(&self) -> Option<&[u8]> {
        range(&self.uri.portText)
    }

    /// The path's segments from `pathHead`, empty where it is null.
    pub(crate) fn path_segments(&self) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut at = self.uri.pathHead;
        // SAFETY: null or a live segment of this URI, each linking the
        // next or null.
        while let Some(segment) = unsafe { at.as_ref() } {
            out.push(range(&segment.text).unwrap_or_default());
            at = segment.next;
        }
        out
    }

    /// `query`, without its `?`.
    pub(crate) fn query(&self) -> Option<&[u8]> {
        range(&self.uri.query)
    }

    /// `fragment`, without its `#`.
    pub(crate) fn fragment(&self) -> Option<&[u8]> {
        range(&self.uri.fragment)
    }

    /// `absolutePath`: "/a" rather than "a", for a URI with no host.
    pub(crate) fn is_absolute_path(&self) -> bool {
        self.uri.absolutePath != 0
    }

    /// `uriHasHostA`.
    pub(crate) fn has_host(&self) -> bool {
        // SAFETY: a live URI.
        unsafe { sys::uriHasHostA(&raw const *self.uri) != 0 }
    }

    /// `uriToStringCharsRequiredA` and `uriToStringA`: the URI recomposed.
    pub(crate) fn recompose(&self) -> Result<Vec<u8>, Error> {
        let mut required: c_int = 0;
        // SAFETY: a live URI and a live count.
        let code =
            unsafe { sys::uriToStringCharsRequiredA(&raw const *self.uri, &raw mut required) };
        Error::check(code)?;
        let len = usize::try_from(required).map_err(|_| Error::Other)?;
        let mut out = vec![0_u8; len + 1];
        let max = required.checked_add(1).ok_or(Error::Other)?;
        // SAFETY: a live URI and `len + 1` writable bytes, which is what
        // `max` says.
        let code = unsafe {
            sys::uriToStringA(
                out.as_mut_ptr().cast(),
                &raw const *self.uri,
                max,
                ptr::null_mut(),
            )
        };
        Error::check(code)?;
        out.truncate(len);
        Ok(out)
    }

    fn set(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
        setter: unsafe extern "C" fn(
            *mut sys::UriUriA,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            *mut c_void,
        ) -> c_int,
    ) -> Result<Result<(), Error>, Thrown> {
        let (first, after) = value.map_or((ptr::null(), ptr::null()), |v| {
            let range = v.as_ptr_range();
            (range.start.cast(), range.end.cast())
        });
        // SAFETY: a URI this owns and a live range or two nulls; the
        // setter copies what it keeps.
        let code = self.call(req, |uri, memory| unsafe {
            setter(uri, first, after, memory)
        })?;
        Ok(Error::check(code))
    }

    /// `uriSetSchemeMmA`; `None` removes the scheme.
    pub(crate) fn set_scheme(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetSchemeMmA)
    }

    /// `uriSetUserInfoMmA`.
    pub(crate) fn set_user_info(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetUserInfoMmA)
    }

    /// `uriSetHostAutoMmA`: an IP address or a registered name, as the
    /// text reads.
    pub(crate) fn set_host(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetHostAutoMmA)
    }

    /// `uriSetPortTextMmA`.
    pub(crate) fn set_port_text(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetPortTextMmA)
    }

    /// `uriSetPathMmA`.
    pub(crate) fn set_path(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetPathMmA)
    }

    /// `uriSetQueryMmA`.
    pub(crate) fn set_query(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetQueryMmA)
    }

    /// `uriSetFragmentMmA`.
    pub(crate) fn set_fragment(
        &mut self,
        req: &mut Request,
        value: Option<&[u8]>,
    ) -> Result<Result<(), Error>, Thrown> {
        self.set(req, value, sys::uriSetFragmentMmA)
    }
}

/// A text range's bytes, `None` where either end is null
/// (`has_text_range`).
fn range(r: &sys::UriTextRangeA) -> Option<&[u8]> {
    if r.first.is_null() || r.afterLast.is_null() {
        return None;
    }
    let len = (r.afterLast as usize).saturating_sub(r.first as usize);
    if len == 0 {
        return Some(&[]);
    }
    // SAFETY: a range of a live URI that owns its text: `len` bytes from
    // `first`.
    Some(unsafe { std::slice::from_raw_parts(r.first.cast(), len) })
}

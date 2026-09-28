//! The binding to the vendored uriparser (design §7.2, #1163;
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

use std::ffi::c_int;
use std::ptr;

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

/// One `UriUriA` owning its text (`owner` set), freed with its members.
/// Boxed, so the struct the library filled never moves.
pub(crate) struct Uri(Box<sys::UriUriA>);

// SAFETY: the URI and every allocation it points at are owned by this
// value alone and reached only through it.
unsafe impl Send for Uri {}

impl Drop for Uri {
    fn drop(&mut self) {
        // SAFETY: a URI this owns, whose members are not reached after.
        unsafe { sys::uriFreeUriMembersMmA(&raw mut *self.0, ptr::null_mut()) };
    }
}

/// A `UriUriA` the library may have half filled, freed on every path out
/// of [`Uri::parse`] that does not keep it.
struct Filling(Box<sys::UriUriA>);

impl Drop for Filling {
    fn drop(&mut self) {
        // SAFETY: a URI zeroed or filled by the library, not reached after.
        unsafe { sys::uriFreeUriMembersMmA(&raw mut *self.0, ptr::null_mut()) };
    }
}

impl Filling {
    fn new() -> Filling {
        Filling(Box::new(sys::UriUriA::empty()))
    }

    fn keep(mut self) -> Uri {
        let uri = std::mem::replace(&mut self.0, Box::new(sys::UriUriA::empty()));
        Uri(uri)
    }
}

impl Uri {
    /// `php_uri_parser_rfc3986_parse_ex` up to its range check:
    /// `uriParseSingleUriExMmA` over `text`, resolved against `base` with
    /// `uriAddBaseUriExMmA` with `URI_RESOLVE_STRICTLY` where there is one, and made
    /// the owner of its text with `uriMakeOwnerMmA`.
    pub(crate) fn parse(text: &[u8], base: Option<&Uri>) -> Result<Uri, Refusal> {
        let mut uri = Filling::new();
        let range = text.as_ptr_range();
        // SAFETY: a zeroed URI, and the live bytes of `text`, which the
        // URI points into until it owns its text below.
        let code = unsafe {
            sys::uriParseSingleUriExMmA(
                &raw mut *uri.0,
                range.start.cast(),
                range.end.cast(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        Error::check(code).map_err(Refusal::Parse)?;
        if let Some(base) = base {
            let mut resolved = Filling::new();
            // SAFETY: a zeroed destination, the parsed URI and a live base,
            // three distinct structs, as the library requires.
            let code = unsafe {
                sys::uriAddBaseUriExMmA(
                    &raw mut *resolved.0,
                    &raw const *uri.0,
                    &raw const *base.0,
                    sys::URI_RESOLVE_STRICTLY,
                    ptr::null_mut(),
                )
            };
            Error::check(code).map_err(Refusal::Resolve)?;
            uri = resolved;
        }
        // SAFETY: a URI pointing into `text` and the base, both live; it
        // owns copies of its text from here.
        let code = unsafe { sys::uriMakeOwnerMmA(&raw mut *uri.0, ptr::null_mut()) };
        Error::check(code).map_err(Refusal::Parse)?;
        Ok(uri.keep())
    }

    /// `copy_uri`: `uriCopyUriMmA` into a fresh URI.
    pub(crate) fn try_clone(&self) -> Result<Uri, Error> {
        let mut copy = Filling::new();
        // SAFETY: a zeroed destination and a live source.
        let code =
            unsafe { sys::uriCopyUriMmA(&raw mut *copy.0, &raw const *self.0, ptr::null_mut()) };
        Error::check(code)?;
        Ok(copy.keep())
    }

    /// `get_normalized_uri`: a copy with every part normalised,
    /// `uriNormalizeSyntaxExMmA` under the mask `(unsigned int)-1`.
    pub(crate) fn normalized(&self) -> Result<Uri, Error> {
        let mut copy = self.try_clone()?;
        // SAFETY: a URI this owns.
        let code =
            unsafe { sys::uriNormalizeSyntaxExMmA(&raw mut *copy.0, u32::MAX, ptr::null_mut()) };
        Error::check(code)?;
        Ok(copy)
    }

    /// `scheme`, `None` without a text range.
    pub(crate) fn scheme(&self) -> Option<&[u8]> {
        range(&self.0.scheme)
    }

    /// `userInfo`.
    pub(crate) fn user_info(&self) -> Option<&[u8]> {
        range(&self.0.userInfo)
    }

    /// `hostText`, which excludes the brackets of an IP address.
    pub(crate) fn host_text(&self) -> Option<&[u8]> {
        range(&self.0.hostText)
    }

    /// Whether the host is an IPv6 or an `IPvFuture` address, which php
    /// shows in its brackets.
    pub(crate) fn host_is_ip_literal(&self) -> bool {
        !self.0.hostData.ip6.is_null() || !self.0.hostData.ipFuture.first.is_null()
    }

    /// `portText`.
    pub(crate) fn port_text(&self) -> Option<&[u8]> {
        range(&self.0.portText)
    }

    /// The path's segments from `pathHead`, empty where it is null.
    pub(crate) fn path_segments(&self) -> Vec<&[u8]> {
        let mut out = Vec::new();
        let mut at = self.0.pathHead;
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
        range(&self.0.query)
    }

    /// `fragment`, without its `#`.
    pub(crate) fn fragment(&self) -> Option<&[u8]> {
        range(&self.0.fragment)
    }

    /// `absolutePath`: "/a" rather than "a", for a URI with no host.
    pub(crate) fn is_absolute_path(&self) -> bool {
        self.0.absolutePath != 0
    }

    /// `uriHasHostA`.
    pub(crate) fn has_host(&self) -> bool {
        // SAFETY: a live URI.
        unsafe { sys::uriHasHostA(&raw const *self.0) != 0 }
    }

    /// `uriToStringCharsRequiredA` and `uriToStringA`: the URI recomposed.
    pub(crate) fn recompose(&self) -> Result<Vec<u8>, Error> {
        let mut required: c_int = 0;
        // SAFETY: a live URI and a live count.
        let code = unsafe { sys::uriToStringCharsRequiredA(&raw const *self.0, &raw mut required) };
        Error::check(code)?;
        let len = usize::try_from(required).map_err(|_| Error::Other)?;
        let mut out = vec![0_u8; len + 1];
        let max = required.checked_add(1).ok_or(Error::Other)?;
        // SAFETY: a live URI and `len + 1` writable bytes, which is what
        // `max` says.
        let code = unsafe {
            sys::uriToStringA(
                out.as_mut_ptr().cast(),
                &raw const *self.0,
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
        value: Option<&[u8]>,
        setter: unsafe extern "C" fn(
            *mut sys::UriUriA,
            *const std::ffi::c_char,
            *const std::ffi::c_char,
            *mut std::ffi::c_void,
        ) -> c_int,
    ) -> Result<(), Error> {
        let (first, after) = value.map_or((ptr::null(), ptr::null()), |v| {
            let range = v.as_ptr_range();
            (range.start.cast(), range.end.cast())
        });
        // SAFETY: a URI this owns and a live range or two nulls; the
        // setter copies what it keeps.
        let code = unsafe { setter(&raw mut *self.0, first, after, ptr::null_mut()) };
        Error::check(code)
    }

    /// `uriSetSchemeMmA`; `None` removes the scheme.
    pub(crate) fn set_scheme(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetSchemeMmA)
    }

    /// `uriSetUserInfoMmA`.
    pub(crate) fn set_user_info(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetUserInfoMmA)
    }

    /// `uriSetHostAutoMmA`: an IP address or a registered name, as the
    /// text reads.
    pub(crate) fn set_host(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetHostAutoMmA)
    }

    /// `uriSetPortTextMmA`.
    pub(crate) fn set_port_text(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetPortTextMmA)
    }

    /// `uriSetPathMmA`.
    pub(crate) fn set_path(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetPathMmA)
    }

    /// `uriSetQueryMmA`.
    pub(crate) fn set_query(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetQueryMmA)
    }

    /// `uriSetFragmentMmA`.
    pub(crate) fn set_fragment(&mut self, value: Option<&[u8]>) -> Result<(), Error> {
        self.set(value, sys::uriSetFragmentMmA)
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

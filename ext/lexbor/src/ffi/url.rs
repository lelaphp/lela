//! lexbor's WHATWG URL parser (`lexbor/url`), as php's
//! `ext/uri/uri_parser_whatwg.c` drives it: a [`Url`] parsed against an
//! optional base, its components read, changed one setter at a time,
//! serialised with the host in ASCII or in Unicode, and the validation
//! errors each parse or setter logs as [`ValidationError`]s.
//!
//! php keeps one parser, one memory pool and one `lxb_unicode_idna_t` per
//! request and allocates every URL in that pool. Here each [`Url`] owns
//! the pool it lives in, so a URL is a value that frees itself and
//! outlives no request state; a parser and an IDNA converter are made for
//! the one call that needs them, as php's `lxb_url_parser_clean` and
//! `lxb_unicode_idna_clean` leave them empty between calls.

use std::ffi::{CStr, c_void};
use std::ptr::{self, NonNull};

use super::sys;

/// The chunk size of a URL's own pool: `LXB_URL_BUFFER_SIZE`, what
/// `lxb_url_parser_init` gives a pool it makes itself.
const POOL_CHUNK: usize = 4096;

/// `lxb_url_error_type_t`: one validation error of the URL standard
/// (<https://url.spec.whatwg.org/#writing>), as lexbor logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    DomainToAscii,
    DomainToUnicode,
    DomainInvalidCodePoint,
    HostInvalidCodePoint,
    Ipv4EmptyPart,
    Ipv4TooManyParts,
    Ipv4NonNumericPart,
    Ipv4NonDecimalPart,
    Ipv4OutOfRangePart,
    Ipv6Unclosed,
    Ipv6InvalidCompression,
    Ipv6TooManyPieces,
    Ipv6MultipleCompression,
    Ipv6InvalidCodePoint,
    Ipv6TooFewPieces,
    Ipv4InIpv6TooManyPieces,
    Ipv4InIpv6InvalidCodePoint,
    Ipv4InIpv6OutOfRangePart,
    Ipv4InIpv6TooFewParts,
    InvalidUrlUnit,
    SpecialSchemeMissingFollowingSolidus,
    MissingSchemeNonRelativeUrl,
    InvalidReverseSolidus,
    InvalidCredentials,
    HostMissing,
    PortOutOfRange,
    PortInvalid,
    FileInvalidWindowsDriveLetter,
    FileInvalidWindowsDriveLetterHost,
}

impl ErrorKind {
    /// Every kind, in `lxb_url_error_type_t`'s order, so a logged id is
    /// its index.
    const ALL: [ErrorKind; 29] = [
        ErrorKind::DomainToAscii,
        ErrorKind::DomainToUnicode,
        ErrorKind::DomainInvalidCodePoint,
        ErrorKind::HostInvalidCodePoint,
        ErrorKind::Ipv4EmptyPart,
        ErrorKind::Ipv4TooManyParts,
        ErrorKind::Ipv4NonNumericPart,
        ErrorKind::Ipv4NonDecimalPart,
        ErrorKind::Ipv4OutOfRangePart,
        ErrorKind::Ipv6Unclosed,
        ErrorKind::Ipv6InvalidCompression,
        ErrorKind::Ipv6TooManyPieces,
        ErrorKind::Ipv6MultipleCompression,
        ErrorKind::Ipv6InvalidCodePoint,
        ErrorKind::Ipv6TooFewPieces,
        ErrorKind::Ipv4InIpv6TooManyPieces,
        ErrorKind::Ipv4InIpv6InvalidCodePoint,
        ErrorKind::Ipv4InIpv6OutOfRangePart,
        ErrorKind::Ipv4InIpv6TooFewParts,
        ErrorKind::InvalidUrlUnit,
        ErrorKind::SpecialSchemeMissingFollowingSolidus,
        ErrorKind::MissingSchemeNonRelativeUrl,
        ErrorKind::InvalidReverseSolidus,
        ErrorKind::InvalidCredentials,
        ErrorKind::HostMissing,
        ErrorKind::PortOutOfRange,
        ErrorKind::PortInvalid,
        ErrorKind::FileInvalidWindowsDriveLetter,
        ErrorKind::FileInvalidWindowsDriveLetterHost,
    ];

    fn from_raw(id: u32) -> Option<ErrorKind> {
        ErrorKind::ALL.get(usize::try_from(id).ok()?).copied()
    }
}

/// One entry of the parser's log, `lexbor_plog_entry_t`: what kind of
/// error, and its `data`, the input from where lexbor stood to its end,
/// as php reads `lxb_error->data` as a C string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationError {
    pub data: Vec<u8>,
    pub kind: ErrorKind,
}

/// What a parse left: the URL, `None` where the input is no URL, and
/// the validation errors logged on the way, in the order php's
/// `fill_errors` pops them, the last logged first.
#[derive(Debug)]
pub struct Parsed {
    pub url: Option<Url>,
    pub errors: Vec<ValidationError>,
}

/// A setter the URL standard refused (`lxb_url_api_*_set` answering
/// other than `LXB_STATUS_OK`), with what the parser logged, the last
/// logged first. The URL may be left part-changed, as lexbor leaves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub Vec<ValidationError>);

/// `lxb_url_host_t`: a URL's host by its `lxb_url_host_type_t`, borrowing
/// the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host<'a> {
    Domain(&'a [u8]),
    Opaque(&'a [u8]),
    Ipv4(u32),
    Ipv6([u16; 8]),
    Empty,
}

/// A memory pool, `lexbor_mraw_t`, owned: destroyed with everything
/// allocated in it.
struct Pool(NonNull<c_void>);

impl Pool {
    fn new() -> Option<Pool> {
        // SAFETY: no argument; the pool is this value's from here.
        let pool = Pool(NonNull::new(unsafe { sys::lexbor_mraw_create() })?);
        // SAFETY: a pool just created, not yet initialised.
        let status = unsafe { sys::lexbor_mraw_init(pool.0.as_ptr(), POOL_CHUNK) };
        (status == sys::LXB_STATUS_OK).then_some(pool)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: a pool this owns; nothing allocated in it is reached
        // after this.
        unsafe { sys::lexbor_mraw_destroy(self.0.as_ptr(), true) };
    }
}

/// A URL parser over one pool, `lxb_url_parser_t`, for one call: its log
/// and buffers are freed on drop, the pool is not.
struct Parser(NonNull<sys::lxb_url_parser_t>);

impl Parser {
    fn new(pool: &Pool) -> Option<Parser> {
        // SAFETY: no argument; the parser is this value's from here.
        let parser = Parser(NonNull::new(unsafe { sys::lxb_url_parser_create() })?);
        // SAFETY: a parser just created, over a live pool that outlives
        // it at every caller.
        let status = unsafe { sys::lxb_url_parser_init(parser.raw(), pool.0.as_ptr()) };
        (status == sys::LXB_STATUS_OK).then_some(parser)
    }

    fn raw(&self) -> *mut sys::lxb_url_parser_t {
        self.0.as_ptr()
    }

    /// The log, popped as php's `fill_errors` pops it: the last entry
    /// first. Read before anything the entries point into is freed.
    fn errors(&self) -> Vec<ValidationError> {
        // SAFETY: a live parser; `log` is its field.
        let log = unsafe { (*self.raw()).log };
        let logged: Vec<&sys::lexbor_plog_entry_t> = super::html::entries(log, 0).collect();
        logged
            .into_iter()
            .rev()
            .filter_map(validation_error)
            .collect()
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: a parser this owns; its log, IDNA context and buffer
        // are freed with it, its pool is not.
        unsafe { sys::lxb_url_parser_destroy(self.raw(), true) };
    }
}

fn validation_error(entry: &sys::lexbor_plog_entry_t) -> Option<ValidationError> {
    let kind = ErrorKind::from_raw(entry.id)?;
    let data = if entry.data.is_null() {
        Vec::new()
    } else {
        // SAFETY: lexbor logs a position in the input, which every caller
        // here hands over NUL-terminated, or in a NUL-terminated copy the
        // parser still holds, as php reads it with `ZVAL_STRING`.
        unsafe { CStr::from_ptr(entry.data.cast()) }
            .to_bytes()
            .to_vec()
    };
    Some(ValidationError { data, kind })
}

/// An IDNA converter, `lxb_unicode_idna_t`, for one serialisation.
struct Idna(NonNull<c_void>);

impl Idna {
    fn new() -> Option<Idna> {
        // SAFETY: no argument; the context is this value's from here.
        let idna = Idna(NonNull::new(unsafe { sys::lxb_unicode_idna_create() })?);
        // SAFETY: a context just created, not yet initialised.
        let status = unsafe { sys::lxb_unicode_idna_init(idna.0.as_ptr()) };
        (status == sys::LXB_STATUS_OK).then_some(idna)
    }
}

impl Drop for Idna {
    fn drop(&mut self) {
        // SAFETY: a context this owns.
        unsafe { sys::lxb_unicode_idna_destroy(self.0.as_ptr(), true) };
    }
}

/// `serialize_to_smart_str_callback`: each piece appended to the
/// `Vec<u8>` behind `ctx`.
unsafe extern "C" fn append(data: *const u8, len: usize, ctx: *mut c_void) -> sys::lxb_status_t {
    if !data.is_null() && len > 0 {
        // SAFETY: lexbor hands `len` live bytes at `data`.
        let piece = unsafe { std::slice::from_raw_parts(data, len) };
        // SAFETY: every caller passes a live `Vec<u8>` as `ctx`.
        unsafe { &mut *ctx.cast::<Vec<u8>>() }.extend_from_slice(piece);
    }
    sys::LXB_STATUS_OK
}

/// `bytes` with a NUL after them, which lexbor's log positions are read
/// up to.
fn terminated(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 1);
    out.extend_from_slice(bytes);
    out.push(0);
    out
}

/// One parsed URL, `lxb_url_t`, in a pool of its own.
pub struct Url {
    url: NonNull<sys::lxb_url_t>,
    /// Declared after `url`, which lives in it.
    pool: Pool,
}

impl std::fmt::Debug for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Url(")?;
        f.write_str(&String::from_utf8_lossy(&self.serialize(false)))?;
        f.write_str(")")
    }
}

// SAFETY: the URL and the pool it lives in are owned by this value alone
// and reached only through it.
unsafe impl Send for Url {}

impl Url {
    /// `lxb_url_parse`: `input` parsed against `base`, as
    /// `php_uri_parser_whatwg_parse_ex` parses it.
    #[must_use]
    pub fn parse(input: &[u8], base: Option<&Url>) -> Parsed {
        let failed = Parsed {
            url: None,
            errors: Vec::new(),
        };
        let Some(pool) = Pool::new() else {
            return failed;
        };
        let Some(parser) = Parser::new(&pool) else {
            return failed;
        };
        let input = terminated(input);
        let base = base.map_or(ptr::null(), |b| b.url.as_ptr().cast_const());
        // SAFETY: a live parser over `pool`, a live base or null, and the
        // live bytes of `input` less its NUL.
        let url =
            unsafe { sys::lxb_url_parse(parser.raw(), base, input.as_ptr(), input.len() - 1) };
        let errors = parser.errors();
        drop(parser);
        Parsed {
            url: NonNull::new(url).map(|url| Url { url, pool }),
            errors,
        }
    }

    /// `lxb_url_clone` into a pool of its own; `None` where lexbor could
    /// not allocate.
    #[must_use]
    pub fn try_clone(&self) -> Option<Url> {
        let pool = Pool::new()?;
        // SAFETY: a live pool and a live URL.
        let url = unsafe { sys::lxb_url_clone(pool.0.as_ptr(), self.url.as_ptr()) };
        Some(Url {
            url: NonNull::new(url)?,
            pool,
        })
    }

    fn get(&self) -> &sys::lxb_url_t {
        // SAFETY: a live URL this owns, borrowed as long as `self` is.
        unsafe { self.url.as_ref() }
    }

    /// `scheme.name`.
    #[must_use]
    pub fn scheme(&self) -> &[u8] {
        str_bytes(&self.get().scheme.name)
    }

    /// `username`.
    #[must_use]
    pub fn username(&self) -> &[u8] {
        str_bytes(&self.get().username)
    }

    /// `password`.
    #[must_use]
    pub fn password(&self) -> &[u8] {
        str_bytes(&self.get().password)
    }

    /// `host`, `None` for `LXB_URL_HOST_TYPE__UNDEF`.
    #[must_use]
    pub fn host(&self) -> Option<Host<'_>> {
        let host = &self.get().host;
        Some(match host.type_ {
            sys::LXB_URL_HOST_TYPE_DOMAIN => {
                // SAFETY: a domain host holds `u.domain`.
                Host::Domain(str_bytes(unsafe { &host.u.domain }))
            }
            sys::LXB_URL_HOST_TYPE_OPAQUE => {
                // SAFETY: an opaque host holds `u.opaque`.
                Host::Opaque(str_bytes(unsafe { &host.u.opaque }))
            }
            sys::LXB_URL_HOST_TYPE_IPV4 => {
                // SAFETY: an IPv4 host holds `u.ipv4`.
                Host::Ipv4(unsafe { host.u.ipv4 })
            }
            sys::LXB_URL_HOST_TYPE_IPV6 => {
                // SAFETY: an IPv6 host holds `u.ipv6`.
                Host::Ipv6(unsafe { host.u.ipv6 })
            }
            sys::LXB_URL_HOST_TYPE_EMPTY => Host::Empty,
            _ => return None,
        })
    }

    /// `port`, where `has_port`.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        let url = self.get();
        url.has_port.then_some(url.port)
    }

    /// `path.str`.
    #[must_use]
    pub fn path(&self) -> &[u8] {
        str_bytes(&self.get().path.str_)
    }

    /// `query`, `None` where its data is null.
    #[must_use]
    pub fn query(&self) -> Option<&[u8]> {
        present(&self.get().query)
    }

    /// `fragment`, `None` where its data is null.
    #[must_use]
    pub fn fragment(&self) -> Option<&[u8]> {
        present(&self.get().fragment)
    }

    /// A setter that takes the parser, over a fresh one on this URL's
    /// pool, with the log read where lexbor refused.
    fn set_with(
        &mut self,
        value: &[u8],
        set: unsafe extern "C" fn(
            *mut sys::lxb_url_t,
            *mut sys::lxb_url_parser_t,
            *const u8,
            usize,
        ) -> sys::lxb_status_t,
    ) -> Result<(), Refused> {
        let parser = Parser::new(&self.pool).ok_or(Refused(Vec::new()))?;
        let value = terminated(value);
        // SAFETY: a live URL this owns, a live parser over its pool, and
        // the live bytes of `value` less its NUL.
        let status = unsafe {
            set(
                self.url.as_ptr(),
                parser.raw(),
                value.as_ptr(),
                value.len() - 1,
            )
        };
        if status == sys::LXB_STATUS_OK {
            Ok(())
        } else {
            Err(Refused(parser.errors()))
        }
    }

    /// `lxb_url_api_protocol_set`.
    pub fn set_protocol(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_protocol_set)
    }

    /// `lxb_url_api_hostname_set`.
    pub fn set_hostname(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_hostname_set)
    }

    /// `lxb_url_api_port_set`; the empty string removes it.
    pub fn set_port(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_port_set)
    }

    /// `lxb_url_api_pathname_set`.
    pub fn set_pathname(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_pathname_set)
    }

    /// `lxb_url_api_search_set`.
    pub fn set_search(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_search_set)
    }

    /// `lxb_url_api_hash_set`.
    pub fn set_hash(&mut self, value: &[u8]) -> Result<(), Refused> {
        self.set_with(value, sys::lxb_url_api_hash_set)
    }

    /// `lxb_url_api_username_set`, which logs nothing.
    pub fn set_username(&mut self, value: &[u8]) -> Result<(), Refused> {
        // SAFETY: a live URL this owns and the live bytes of `value`.
        let status = unsafe {
            sys::lxb_url_api_username_set(self.url.as_ptr(), value.as_ptr(), value.len())
        };
        if status == sys::LXB_STATUS_OK {
            Ok(())
        } else {
            Err(Refused(Vec::new()))
        }
    }

    /// `lxb_url_api_password_set`, which logs nothing.
    pub fn set_password(&mut self, value: &[u8]) -> Result<(), Refused> {
        // SAFETY: a live URL this owns and the live bytes of `value`.
        let status = unsafe {
            sys::lxb_url_api_password_set(self.url.as_ptr(), value.as_ptr(), value.len())
        };
        if status == sys::LXB_STATUS_OK {
            Ok(())
        } else {
            Err(Refused(Vec::new()))
        }
    }

    /// `lxb_url_serialize`: the URL with its host in ASCII.
    #[must_use]
    pub fn serialize(&self, exclude_fragment: bool) -> Vec<u8> {
        let mut out = Vec::new();
        // SAFETY: a live URL, and a live `Vec<u8>` for `append`.
        unsafe {
            sys::lxb_url_serialize(
                self.url.as_ptr(),
                append,
                ptr::from_mut(&mut out).cast(),
                exclude_fragment,
            )
        };
        out
    }

    /// `lxb_url_serialize_idna`: the URL with its host in Unicode.
    #[must_use]
    pub fn serialize_idna(&self, exclude_fragment: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let Some(idna) = Idna::new() else {
            return self.serialize(exclude_fragment);
        };
        // SAFETY: a live IDNA context, a live URL, and a live `Vec<u8>`
        // for `append`.
        unsafe {
            sys::lxb_url_serialize_idna(
                idna.0.as_ptr(),
                self.url.as_ptr(),
                append,
                ptr::from_mut(&mut out).cast(),
                exclude_fragment,
            )
        };
        out
    }

    /// `lxb_url_serialize_host_unicode`: the host with a domain turned to
    /// Unicode.
    #[must_use]
    pub fn host_unicode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let Some(idna) = Idna::new() else {
            return out;
        };
        // SAFETY: a live IDNA context, the live host of a live URL, and a
        // live `Vec<u8>` for `append`.
        unsafe {
            sys::lxb_url_serialize_host_unicode(
                idna.0.as_ptr(),
                &raw const self.get().host,
                append,
                ptr::from_mut(&mut out).cast(),
            )
        };
        out
    }
}

/// `lxb_url_serialize_host_ipv4`: dotted decimal.
///
/// ```php
/// var_dump((new Uri\WhatWg\Url("https://0x7f.1"))->getAsciiHost(), (new Uri\WhatWg\Url("https://4294967295/"))->toAsciiString());
/// ```
/// ```expect
/// string(9) "127.0.0.1"
/// string(24) "https://255.255.255.255/"
/// ```
#[must_use]
pub fn ipv4_text(ipv4: u32) -> Vec<u8> {
    let mut out = Vec::new();
    // SAFETY: a live `Vec<u8>` for `append`.
    unsafe { sys::lxb_url_serialize_host_ipv4(ipv4, append, ptr::from_mut(&mut out).cast()) };
    out
}

/// `lxb_url_serialize_host_ipv6`: the compressed form, without brackets.
///
/// ```php
/// var_dump((new Uri\WhatWg\Url("https://[0:0:0:0:0:ffff:7f00:1]"))->getAsciiHost(), (new Uri\WhatWg\Url("https://[2001:db8:0:0:1:0:0:1]/"))->toAsciiString());
/// ```
/// ```expect
/// string(15) "[::ffff:7f00:1]"
/// string(28) "https://[2001:db8::1:0:0:1]/"
/// ```
#[must_use]
pub fn ipv6_text(ipv6: &[u16; 8]) -> Vec<u8> {
    let mut out = Vec::new();
    // SAFETY: eight live pieces, and a live `Vec<u8>` for `append`.
    unsafe {
        sys::lxb_url_serialize_host_ipv6(ipv6.as_ptr(), append, ptr::from_mut(&mut out).cast())
    };
    out
}

fn str_bytes(s: &sys::lexbor_str_t) -> &[u8] {
    if s.data.is_null() || s.length == 0 {
        return &[];
    }
    // SAFETY: a lexbor string of a live URL: `length` bytes at `data`.
    unsafe { std::slice::from_raw_parts(s.data, s.length) }
}

fn present(s: &sys::lexbor_str_t) -> Option<&[u8]> {
    (!s.data.is_null()).then(|| str_bytes(s))
}

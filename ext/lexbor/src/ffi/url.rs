//! lexbor's WHATWG URL parser (`lexbor/url`), as php's
//! `ext/uri/uri_parser_whatwg.c` drives it: a [`Url`] parsed against an
//! optional base, its components read, changed one setter at a time,
//! serialised with the host in ASCII or in Unicode, and the validation
//! errors each parse or setter logs as [`ValidationError`]s.
//!
//! php keeps one memory pool, one parser over it and one
//! `lxb_unicode_idna_t` per request, laid at `RINIT` and destroyed at
//! `post_deactivate` (`Z/ext/uri/uri_parser_whatwg.c:533-577`), and
//! allocates every URL of the request in that pool. So does this: the
//! three are a [`Parser`], the request's, and a [`Url`] is a URL in its
//! pool, given back to it by [`Parser::free`] as php's
//! `php_uri_parser_whatwg_destroy` gives it back. Whatever can allocate
//! is a method of the parser and takes the lend's proof ([`Lent`]); a
//! URL's own reads allocate nothing.

use std::ffi::{CStr, c_void};
use std::ptr::{self, NonNull};

use super::{Lent, Owned, sys};

/// `lexbor_mraw_byte_size`, the chunk size of the request's pool.
const POOL_CHUNK: usize = 8192;

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

/// The request's URL parser, php's three `ZEND_TLS` values of
/// `uri_parser_whatwg.c`: `lexbor_mraw`, the pool every URL of the
/// request is allocated in; `lexbor` over it, whose log a parse
/// or setter reads; and `lexbor_idna`, the converter a Unicode read
/// uses. The three structs are this value's own, outside lexbor's
/// allocator as php's are outside `emalloc`, and boxed so the pointers
/// lexbor keeps between them hold wherever the value moves. Laid by
/// [`Parser::new`] and given back as an [`Owned`], each under a
/// lend; a parser dropped without being given back ([`Owned`]) leaves
/// what lexbor holds to its request's end.
pub struct Parser {
    mraw: Box<sys::lexbor_mraw_t>,
    lexbor: Box<sys::lxb_url_parser_t>,
    idna: Box<sys::lxb_unicode_idna_t>,
}

// SAFETY: the three structs and everything lexbor hangs off them are this
// value's and its URLs', which belong to one request and are reached on
// its thread alone.
unsafe impl Send for Parser {}

impl Parser {
    /// `PHP_RINIT_FUNCTION(uri_parser_whatwg)`: the pool laid with
    /// `lexbor_mraw_byte_size`, the parser over it and the IDNA
    /// converter; `None` where lexbor refused one, what was laid
    /// destroyed as php's `fail:` destroys it.
    #[must_use]
    pub fn new(_: &Lent) -> Option<Parser> {
        let mut made = Parser {
            mraw: Box::new(sys::lexbor_mraw_t {
                mem: ptr::null_mut(),
                cache: ptr::null_mut(),
                ref_count: 0,
            }),
            lexbor: Box::new(sys::lxb_url_parser_t {
                url: ptr::null_mut(),
                mraw: ptr::null_mut(),
                log: ptr::null_mut(),
                idna: ptr::null_mut(),
                buffer: ptr::null_mut(),
            }),
            idna: Box::new(sys::lxb_unicode_idna_t {
                decomposition: ptr::null_mut(),
                composition: ptr::null_mut(),
                starter: ptr::null_mut(),
                buf: ptr::null_mut(),
                end: ptr::null(),
                p: ptr::null_mut(),
                ican: ptr::null_mut(),
                tmp: [0; 4],
                tmp_lenght: 0,
                quick_ccc: 0,
                quick_type: 0,
                flush_cp: 0,
            }),
        };
        // SAFETY: a zeroed pool this owns, laid once.
        let laid = unsafe { sys::lexbor_mraw_init(&raw mut *made.mraw, POOL_CHUNK) }
            == sys::LXB_STATUS_OK
            // SAFETY: a zeroed parser this owns, over the pool just laid,
            // which the box keeps in place as long as the parser.
            && unsafe { sys::lxb_url_parser_init(&raw mut *made.lexbor, &raw mut *made.mraw) }
                == sys::LXB_STATUS_OK
            // SAFETY: a zeroed converter this owns, laid once.
            && unsafe { sys::lxb_unicode_idna_init(&raw mut *made.idna) } == sys::LXB_STATUS_OK;
        if laid {
            Some(made)
        } else {
            made.destroy();
            None
        }
    }

    /// The three destroyed, none freed itself, as php destroys them,
    /// and safe on a zeroed one, as php's `fail:` relies on.
    fn destroy(&mut self) {
        // SAFETY: a converter this owns, laid or zeroed, not freed itself.
        unsafe { sys::lxb_unicode_idna_destroy(&raw mut *self.idna, false) };
        // SAFETY: a parser this owns, laid or zeroed; its log, IDNA
        // context and buffer go with it, its pool does not.
        unsafe { sys::lxb_url_parser_destroy(&raw mut *self.lexbor, false) };
        // SAFETY: the pool this owns, laid or zeroed, with every URL in
        // it; nothing allocated in it is reached after this.
        unsafe { sys::lexbor_mraw_destroy(&raw mut *self.mraw, false) };
    }

    fn raw(&mut self) -> *mut sys::lxb_url_parser_t {
        &raw mut *self.lexbor
    }

    /// `lxb_url_parser_clean`, which php makes before every parse and
    /// setter: the log emptied and the buffer freed.
    fn clean(&mut self) {
        // SAFETY: the live parser this owns.
        unsafe { sys::lxb_url_parser_clean(self.raw()) };
    }

    /// The log, popped as php's `fill_errors` pops it: the last entry
    /// first. Read before anything the entries point into is freed.
    fn errors(&self) -> Vec<ValidationError> {
        let logged: Vec<&sys::lexbor_plog_entry_t> =
            super::html::entries(self.lexbor.log, 0).collect();
        logged
            .into_iter()
            .rev()
            .filter_map(validation_error)
            .collect()
    }

    /// `php_uri_parser_whatwg_parse_ex`'s `lxb_url_parse`: `input` parsed
    /// against `base` into the request's pool, the parser cleaned first.
    #[must_use]
    pub fn parse(&mut self, _: &Lent, input: &[u8], base: Option<&Url>) -> Parsed {
        self.clean();
        let input = terminated(input);
        let base = base.map_or(ptr::null(), |b| b.url.as_ptr().cast_const());
        // SAFETY: the live parser over the request's pool, a live base or
        // null, and the live bytes of `input` less its NUL.
        let url = unsafe { sys::lxb_url_parse(self.raw(), base, input.as_ptr(), input.len() - 1) };
        Parsed {
            url: NonNull::new(url).map(|url| Url { url }),
            errors: self.errors(),
        }
    }

    /// `php_uri_parser_whatwg_clone`: `lxb_url_clone` into the request's
    /// pool; `None` where lexbor could not allocate.
    #[must_use]
    pub fn clone_url(&mut self, _: &Lent, url: &Url) -> Option<Url> {
        // SAFETY: the request's pool and a live URL in it.
        let copy = unsafe { sys::lxb_url_clone(&raw mut *self.mraw, url.url.as_ptr()) };
        NonNull::new(copy).map(|url| Url { url })
    }

    /// `php_uri_parser_whatwg_destroy`: `lxb_url_destroy`, the URL's
    /// memory given back to the request's pool.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "taken, so the URL given back is reached no more"
    )]
    pub fn free(&mut self, _: &Lent, url: Url) {
        // SAFETY: a live URL of the request's pool, given back once: the
        // value is taken.
        unsafe { sys::lxb_url_destroy(url.url.as_ptr()) };
    }

    /// A setter that takes the parser, after `lxb_url_parser_clean`, with
    /// the log read where lexbor refused.
    fn set_with(
        &mut self,
        url: &mut Url,
        value: &[u8],
        set: unsafe extern "C" fn(
            *mut sys::lxb_url_t,
            *mut sys::lxb_url_parser_t,
            *const u8,
            usize,
        ) -> sys::lxb_status_t,
    ) -> Result<(), Refused> {
        let value = terminated(value);
        self.clean();
        // SAFETY: a live URL of the request's pool, the live parser, and
        // the live bytes of `value` less its NUL.
        let status = unsafe {
            set(
                url.url.as_ptr(),
                self.raw(),
                value.as_ptr(),
                value.len() - 1,
            )
        };
        self.answer(status)
    }

    /// What a setter answered, the log read where it refused.
    fn answer(&self, status: sys::lxb_status_t) -> Result<(), Refused> {
        if status == sys::LXB_STATUS_OK {
            Ok(())
        } else {
            Err(Refused(self.errors()))
        }
    }

    /// `lxb_url_api_protocol_set`.
    pub fn set_protocol(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.set_with(url, value, sys::lxb_url_api_protocol_set)
    }

    /// `lxb_url_api_hostname_set`.
    pub fn set_hostname(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.set_with(url, value, sys::lxb_url_api_hostname_set)
    }

    /// `lxb_url_api_port_set`; the empty string removes it. A number's
    /// text is copied into the request's pool first, as php's
    /// `zval_long_or_null_to_lexbor_str` copies it there.
    pub fn set_port(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        if value.is_empty() {
            return self.set_with(url, value, sys::lxb_url_api_port_set);
        }
        let mut text = sys::lexbor_str_t {
            data: ptr::null_mut(),
            length: 0,
        };
        // SAFETY: an empty string, the request's pool and the live bytes
        // of `value`; the copy is the pool's, NUL-terminated.
        let copied = unsafe {
            sys::lexbor_str_init_append(
                &raw mut text,
                &raw mut *self.mraw,
                value.as_ptr(),
                value.len(),
            )
        };
        if copied.is_null() {
            return Err(Refused(Vec::new()));
        }
        self.clean();
        // SAFETY: a live URL of the request's pool, the live parser, and
        // the copy's `length` bytes in the pool.
        let status = unsafe {
            sys::lxb_url_api_port_set(url.url.as_ptr(), self.raw(), text.data, text.length)
        };
        self.answer(status)
    }

    /// `lxb_url_api_pathname_set`.
    pub fn set_pathname(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.set_with(url, value, sys::lxb_url_api_pathname_set)
    }

    /// `lxb_url_api_search_set`.
    pub fn set_search(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.set_with(url, value, sys::lxb_url_api_search_set)
    }

    /// `lxb_url_api_hash_set`.
    pub fn set_hash(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.set_with(url, value, sys::lxb_url_api_hash_set)
    }

    /// `lxb_url_api_username_set`, after `lxb_url_parser_clean`; it logs
    /// nothing.
    pub fn set_username(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.clean();
        // SAFETY: a live URL of the request's pool and the live bytes of
        // `value`.
        let status =
            unsafe { sys::lxb_url_api_username_set(url.url.as_ptr(), value.as_ptr(), value.len()) };
        self.answer(status)
    }

    /// `lxb_url_api_password_set`, after `lxb_url_parser_clean`; it logs
    /// nothing.
    pub fn set_password(&mut self, _: &Lent, url: &mut Url, value: &[u8]) -> Result<(), Refused> {
        self.clean();
        // SAFETY: a live URL of the request's pool and the live bytes of
        // `value`.
        let status =
            unsafe { sys::lxb_url_api_password_set(url.url.as_ptr(), value.as_ptr(), value.len()) };
        self.answer(status)
    }

    /// `lxb_url_serialize_idna` over the request's converter, then
    /// `lxb_unicode_idna_clean`: the URL with its host in Unicode.
    #[must_use]
    pub fn serialize_idna(&mut self, _: &Lent, url: &Url, exclude_fragment: bool) -> Vec<u8> {
        let mut out = Vec::new();
        // SAFETY: the live converter, a live URL, and a live `Vec<u8>`
        // for `append`.
        unsafe {
            sys::lxb_url_serialize_idna(
                &raw mut *self.idna,
                url.url.as_ptr(),
                append,
                ptr::from_mut(&mut out).cast(),
                exclude_fragment,
            )
        };
        // SAFETY: the live converter.
        unsafe { sys::lxb_unicode_idna_clean(&raw mut *self.idna) };
        out
    }

    /// `lxb_url_serialize_host_unicode` over the request's converter, then
    /// `lxb_unicode_idna_clean`: the host with a domain turned to
    /// Unicode.
    #[must_use]
    pub fn host_unicode(&mut self, _: &Lent, url: &Url) -> Vec<u8> {
        let mut out = Vec::new();
        // SAFETY: the live converter, the live host of a live URL, and a
        // live `Vec<u8>` for `append`.
        unsafe {
            sys::lxb_url_serialize_host_unicode(
                &raw mut *self.idna,
                &raw const url.get().host,
                append,
                ptr::from_mut(&mut out).cast(),
            )
        };
        // SAFETY: the live converter.
        unsafe { sys::lxb_unicode_idna_clean(&raw mut *self.idna) };
        out
    }
}

impl Owned for Parser {
    /// `ZEND_MODULE_POST_ZEND_DEACTIVATE_D(uri_parser_whatwg)`: the
    /// converter, the parser and the pool destroyed in php's order, with
    /// every URL still in the pool.
    fn give_back(mut self, _: &Lent) {
        self.destroy();
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

/// One parsed URL, `lxb_url_t`, in the request's pool: valid while the
/// [`Parser`] that made it stands, which the request's end destroys after
/// its objects are freed (ADR-0010's `post_deactivate`). It has no drop
/// of its own: [`Parser::free`] gives it back to the pool, and one never
/// given back goes with the pool.
pub struct Url {
    url: NonNull<sys::lxb_url_t>,
}

impl std::fmt::Debug for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Url(")?;
        f.write_str(&String::from_utf8_lossy(&self.serialize(false)))?;
        f.write_str(")")
    }
}

// SAFETY: the URL is its request's, in the request's pool, reached on
// the request's thread alone and only while the pool stands.
unsafe impl Send for Url {}

impl Url {
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

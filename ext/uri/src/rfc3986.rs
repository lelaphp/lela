//! `uri_parser_rfc3986.c` and the `Uri\Rfc3986\Uri` methods of
//! `php_uri.c`: RFC 3986 over uriparser ([`ffi::Uri`]), the URI as
//! parsed beside its normalized copy, made on first read and dropped by
//! every write.

use php_ext::abi::{Instance, Static, This};
use php_ext::attr::php_class;
use php_ext::raising::{Thrown, throw, throw_error};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString};

use crate::ffi::{self, Refusal};
use crate::throwables::Throwables;
use crate::{
    Handlers, Parsed, Read, ReadAs, Recomposition, Slot, Unreadable, UriComparisonMode, UriParser,
};

/// `php_uri_parser_rfc3986_uris`: the URI as parsed or written, and its
/// normalized copy once a read has asked for it, in the struct
/// `uriparser_create_uris` `ecalloc`s on the request's heap for them,
/// and their members in what uriparser allocated through php's memory
/// manager (design §5.13).
///
/// ```php
/// $w = new Uri\Rfc3986\Uri('https://example.com/a?b#c'); $w->getHost();
/// $m = memory_get_usage(); $u = new Uri\Rfc3986\Uri('https://example.com/a?b#c'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $h = $u->getHost(); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v = $u->withPath('/x/y'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); unset($u, $v); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $r = Uri\Rfc3986\Uri::parse('../c', new Uri\Rfc3986\Uri('https://example.com/a/b')); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 576
/// 152
/// 624
/// -1312
/// 544
/// ```
pub(crate) struct Rfc3986(Box<Uris>);

/// [`Rfc3986`]'s fields, boxed, so the payload fits the `php_uri_object`
/// php allocates.
struct Uris {
    uri: ffi::Uri,
    normalized: Option<ffi::Uri>,
    /// The struct's own allocation, `sizeof(php_uri_parser_rfc3986_uris)`.
    allocation: Emalloc,
}

/// `sizeof(php_uri_parser_rfc3986_uris)`: two `UriUriA` and `normalized_uri_initialized`.
const URIS: usize = 328;

/// `MAX_LENGTH_OF_LONG` less its sign, the longest `portText` php reads.
const MAX_PORT_TEXT: usize = 20;

/// `port_str_to_zend_long_checked`: the `portText` as a long, `None` for
/// one longer than `MAX_LENGTH_OF_LONG` or past `ZEND_LONG_MAX`.
fn port_value(text: &[u8]) -> Option<i64> {
    if text.len() > MAX_PORT_TEXT {
        return None;
    }
    let mut value: u64 = 0;
    for &digit in text.iter().take_while(|b| b.is_ascii_digit()) {
        value = value
            .checked_mul(10)?
            .checked_add(u64::from(digit - b'0'))?;
    }
    i64::try_from(value).ok()
}

/// The throwable a refused write or parse makes pending: php's
/// `InvalidUriException` for what the text got wrong, its `UriError` for
/// what "should be unreachable in practice".
fn refused(req: &mut Request, invalid: Option<&str>, other: &str) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    match invalid {
        Some(message) => throw(req, ce.invalid_uri_exception, message),
        None => throw(req, ce.uri_error, other),
    }
}

impl Rfc3986 {
    /// `get_uri_for_reading`: the URI as parsed for a raw read, its
    /// normalized copy, which [`UriParser::prepare`] made, for any
    /// other.
    fn reading(&mut self, mode: ReadAs) -> Read<&ffi::Uri> {
        if mode == ReadAs::Raw {
            return Ok(&self.0.uri);
        }
        self.0.normalized.as_ref().ok_or(Unreadable)
    }

    /// `php_uri_parser_rfc3986_host_read`: an IP address in its brackets.
    fn host_read(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        let uri = self.reading(mode)?;
        let Some(text) = uri.host_text() else {
            return Ok(None);
        };
        if !uri.host_is_ip_literal() {
            return Ok(Some(text.to_vec()));
        }
        let mut host = Vec::with_capacity(text.len() + 2);
        host.push(b'[');
        host.extend_from_slice(text);
        host.push(b']');
        Ok(Some(host))
    }

    /// `get_uri_for_writing`: the URI as parsed, its normalized copy
    /// given back, since the write makes it stale.
    fn writing(&mut self, req: &mut Request) -> &mut ffi::Uri {
        if let Some(normalized) = self.0.normalized.take() {
            normalized.free(req);
        }
        &mut self.0.uri
    }

    /// `php_uri_parser_rfc3986_userinfo_read`.
    fn user_info(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.reading(mode)?.user_info().map(<[u8]>::to_vec))
    }

    /// `php_uri_parser_rfc3986_userinfo_write`.
    fn set_user_info(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        self.writing(req)
            .set_user_info(req, value)?
            .map_err(|error| {
                let invalid = match error {
                    ffi::Error::UserInfoWithoutHost => {
                        Some("Cannot set a userinfo without having a host")
                    }
                    ffi::Error::Syntax => Some("The specified userinfo is malformed"),
                    _ => None,
                };
                refused(req, invalid, "Failed to update the userinfo")
            })
    }
}

impl UriParser for Rfc3986 {
    const CLASS: &'static str = "Uri\\Rfc3986\\Uri";

    /// `php_uri_parser_rfc3986_parse_ex`.
    ///
    /// Since 8.5.12 uriparser's memory manager asks fallibly: past the
    /// memory limit an allocation is refused, the parse fails, and the limit's
    /// fatal is raised as the call returns, for a path's segments and for the
    /// copy of the text the URI comes to own:
    ///
    /// ```php
    /// $t = 'https://example.com/' . str_repeat('a/', 1 << 20);
    /// ini_set('memory_limit', '8M');
    /// $u = Uri\Rfc3986\Uri::parse($t);
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 4
    /// ```
    ///
    /// ```php
    /// $t = 'https://example.com/' . str_repeat('a', 3 << 20);
    /// ini_set('memory_limit', '8M');
    /// $u = new Uri\Rfc3986\Uri($t);
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 3145728 bytes) in %s on line 4
    /// ```
    fn parse(
        req: &mut Request,
        text: &[u8],
        base: Option<&Self>,
        _want_errors: bool,
        silent: bool,
    ) -> Result<Parsed<Self>, Thrown> {
        let failed = Parsed {
            uri: None,
            errors: None,
        };
        let uri = match ffi::Uri::parse(req, text, base.map(|base| &base.0.uri))? {
            Ok(uri) => uri,
            Err(_) if silent => return Ok(failed),
            Err(refusal) => {
                return Err(match refusal {
                    Refusal::Parse(ffi::Error::Syntax) => {
                        refused(req, Some("The specified URI is malformed"), "")
                    }
                    Refusal::Parse(_) => refused(req, None, "Failed to parse the specified URI"),
                    Refusal::Resolve(ffi::Error::RelativeBase) => {
                        refused(req, Some("The specified base URI must be absolute"), "")
                    }
                    Refusal::Resolve(_) => refused(
                        req,
                        None,
                        "Failed to resolve the specified URI against the base URI",
                    ),
                });
            }
        };
        if let Some(port) = uri.port_text()
            && !port.is_empty()
            && port_value(port).is_none()
        {
            uri.free(req);
            if silent {
                return Ok(failed);
            }
            return Err(refused(req, Some("The port is out of range"), ""));
        }
        let uris = match Emalloc::new(req, URIS) {
            Ok(uris) => uris,
            Err(thrown) => {
                uri.free(req);
                return Err(thrown);
            }
        };
        Ok(Parsed {
            uri: Some(Rfc3986(Box::new(Uris {
                uri,
                normalized: None,
                allocation: uris,
            }))),
            errors: None,
        })
    }

    /// `php_uri_parser_rfc3986_clone`: the struct made and the URI copied
    /// into it, the normalized copy not, since a write is what follows a
    /// clone.
    fn duplicate(&self, req: &mut Request) -> Result<Option<Self>, Thrown> {
        let uris = Emalloc::new(req, URIS)?;
        match self.0.uri.try_clone(req) {
            Ok(Ok(uri)) => Ok(Some(Rfc3986(Box::new(Uris {
                uri,
                normalized: None,
                allocation: uris,
            })))),
            Ok(Err(_)) => {
                uris.free(req);
                Ok(None)
            }
            Err(thrown) => {
                uris.free(req);
                Err(thrown)
            }
        }
    }

    /// `get_normalized_uri`'s copy, made on the first read that is not
    /// raw and kept until a write; a copy the library could not make
    /// leaves the read unreadable.
    fn prepare(&mut self, req: &mut Request, mode: ReadAs) -> Result<(), Thrown> {
        if mode == ReadAs::Raw || self.0.normalized.is_some() {
            return Ok(());
        }
        self.0.normalized = self.0.uri.normalized(req)?.ok();
        Ok(())
    }

    /// `php_uri_parser_rfc3986_destroy`: both URIs' members and the
    /// struct given back.
    fn free(self, req: &mut Request) {
        let Uris {
            uri,
            normalized,
            allocation,
        } = *self.0;
        uri.free(req);
        if let Some(normalized) = normalized {
            normalized.free(req);
        }
        allocation.free(req);
    }

    /// `php_uri_parser_rfc3986_to_string`: the fragment cut at the last
    /// `#` where it is excluded.
    fn recompose(
        &mut self,
        _: &mut Request,
        mode: Recomposition,
        exclude_fragment: bool,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let read = match mode {
            Recomposition::RawAscii | Recomposition::RawUnicode => ReadAs::Raw,
            Recomposition::NormalizedAscii => ReadAs::NormalizedAscii,
        };
        let text = self.reading(read).ok().and_then(|uri| uri.recompose().ok());
        Ok(text.map(|mut text| {
            if exclude_fragment && let Some(hash) = text.iter().rposition(|&b| b == b'#') {
                text.truncate(hash);
            }
            text
        }))
    }

    fn scheme(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.reading(mode)?.scheme().map(<[u8]>::to_vec))
    }

    /// `php_uri_parser_rfc3986_username_read`: the user info up to its
    /// first `:`.
    fn username(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        let Some(info) = self.reading(mode)?.user_info() else {
            return Ok(None);
        };
        let name = match info.iter().position(|&b| b == b':') {
            Some(colon) => &info[..colon],
            None => info,
        };
        Ok(Some(name.to_vec()))
    }

    /// `php_uri_parser_rfc3986_password_read`: the user info after its
    /// first `:`, empty without one.
    fn password(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        let Some(info) = self.reading(mode)?.user_info() else {
            return Ok(None);
        };
        let password = match info.iter().position(|&b| b == b':') {
            Some(colon) => &info[colon + 1..],
            None => &[],
        };
        Ok(Some(password.to_vec()))
    }

    /// [`Rfc3986::host_read`], which allocates nothing of the request's.
    fn host(&mut self, _: &mut Request, mode: ReadAs) -> Result<Read<Option<Vec<u8>>>, Thrown> {
        Ok(self.host_read(mode))
    }

    fn port(&mut self, mode: ReadAs) -> Read<Option<i64>> {
        let port = self.reading(mode)?.port_text();
        Ok(port
            .filter(|port| !port.is_empty())
            .map(|port| port_value(port).unwrap_or(-1)))
    }

    /// `php_uri_parser_rfc3986_path_read`: the segments joined by `/`,
    /// led by one for an absolute path or a URI with a host.
    fn path(&mut self, mode: ReadAs) -> Read<Vec<u8>> {
        let uri = self.reading(mode)?;
        let segments = uri.path_segments();
        if segments.is_empty() {
            return Ok(if uri.is_absolute_path() {
                b"/".to_vec()
            } else {
                Vec::new()
            });
        }
        let mut path = Vec::new();
        if uri.is_absolute_path() || uri.has_host() {
            path.push(b'/');
        }
        path.extend_from_slice(&segments.join(&b'/'));
        Ok(path)
    }

    fn query(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.reading(mode)?.query().map(<[u8]>::to_vec))
    }

    fn fragment(&mut self, mode: ReadAs) -> Read<Option<Vec<u8>>> {
        Ok(self.reading(mode)?.fragment().map(<[u8]>::to_vec))
    }

    fn set_scheme(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        self.writing(req).set_scheme(req, value)?.map_err(|error| {
            let invalid =
                (error == ffi::Error::Syntax).then_some("The specified scheme is malformed");
            refused(req, invalid, "Failed to update the scheme")
        })
    }

    fn set_host(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        self.writing(req).set_host(req, value)?.map_err(|error| {
            let invalid = match error {
                ffi::Error::HostRemovedUnderPort => {
                    Some("Cannot remove the host from a URI that has a port")
                }
                ffi::Error::HostRemovedUnderUserInfo => {
                    Some("Cannot remove the host from a URI that has a userinfo")
                }
                ffi::Error::Syntax => Some("The specified host is malformed"),
                _ => None,
            };
            refused(req, invalid, "Failed to update the host")
        })
    }

    fn set_port(&mut self, req: &mut Request, value: Option<i64>) -> Result<(), Thrown> {
        let text = value.map(|port| port.to_string().into_bytes());
        self.writing(req)
            .set_port_text(req, text.as_deref())?
            .map_err(|error| {
                let invalid = match error {
                    ffi::Error::PortWithoutHost => Some("Cannot set a port without having a host"),
                    ffi::Error::Syntax => Some("The specified port is malformed"),
                    _ => None,
                };
                refused(req, invalid, "Failed to update the port")
            })
    }

    fn set_path(&mut self, req: &mut Request, value: &[u8]) -> Result<(), Thrown> {
        let value = (!value.is_empty()).then_some(value);
        self.writing(req).set_path(req, value)?.map_err(|error| {
            let invalid =
                (error == ffi::Error::Syntax).then_some("The specified path is malformed");
            refused(req, invalid, "Failed to update the path")
        })
    }

    fn set_query(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        self.writing(req).set_query(req, value)?.map_err(|error| {
            let invalid =
                (error == ffi::Error::Syntax).then_some("The specified query is malformed");
            refused(req, invalid, "Failed to update the query")
        })
    }

    fn set_fragment(&mut self, req: &mut Request, value: Option<&[u8]>) -> Result<(), Thrown> {
        self.writing(req)
            .set_fragment(req, value)?
            .map_err(|error| {
                let invalid =
                    (error == ffi::Error::Syntax).then_some("The specified fragment is malformed");
                refused(req, invalid, "Failed to update the fragment")
            })
    }
}

/// The handlers of `Uri\Rfc3986\Uri`.
pub(crate) static HANDLERS: Handlers<Rfc3986> = Handlers(std::marker::PhantomData);

/// `final readonly class Uri\Rfc3986\Uri`, `@strict-properties`: one
/// RFC 3986 URI, immutable, each `with*()` a changed copy.
#[php_class(
    final,
    readonly,
    namespace(Uri::Rfc3986),
    dynamic_properties = forbidden,
    size = 72, payload = Slot<Rfc3986>,
    handlers = crate::rfc3986::HANDLERS
)]
pub(crate) struct Uri;

type R = Rfc3986;

#[php_class]
impl Uri {
    /// `Uri\Rfc3986\Uri::parse(string $uri, ?Uri\Rfc3986\Uri $baseUrl =
    /// null): ?static`: the URI, or `null` where the text is not one.
    ///
    /// ```php
    /// var_dump(Uri\Rfc3986\Uri::parse("https://example.com/a")?->toString(), Uri\Rfc3986\Uri::parse("https://exa mple.com"));
    /// var_dump(Uri\Rfc3986\Uri::parse("../c", Uri\Rfc3986\Uri::parse("https://example.com/a/b"))->toString());
    /// var_dump(Uri\Rfc3986\Uri::parse("c", Uri\Rfc3986\Uri::parse("/a/b")));
    /// ```
    /// ```expect
    /// string(21) "https://example.com/a"
    /// NULL
    /// string(21) "https://example.com/c"
    /// NULL
    /// ```
    fn parse(
        req: &mut Request,
        uri: PhpString,
        #[php(default = null)] baseUrl: Option<Instance<Uri>>,
    ) -> Result<Option<Owner<Static>>, Thrown> {
        crate::parse_static::<R>(req, uri, baseUrl.map(|base| base.object()), None)
    }

    /// `Uri\Rfc3986\Uri::__construct(string $uri, ?Uri\Rfc3986\Uri
    /// $baseUrl = null)`: the URI, or php's `InvalidUriException`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("HTTPS://Example.COM:0443/%7euser"))->toRawString());
    /// foreach (["https://exa mple.com", "https://example.com:99999999999999999999"] as $u) {
    ///     try { new Uri\Rfc3986\Uri($u); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// try { new Uri\Rfc3986\Uri("a", new Uri\Rfc3986\Uri("/b")); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(32) "HTTPS://Example.COM:0443/%7euser"
    /// The specified URI is malformed
    /// The port is out of range
    /// The specified base URI must be absolute
    /// ```
    ///
    /// The raw and the normalized components read side by side, the
    /// references of RFC 3986 §5.4.1 resolved against its base, and an
    /// absent component apart from an empty one, as uriparser reads them.
    ///
    /// ```php
    /// use Uri\Rfc3986\Uri as R;
    /// $r = new R('HTTP://Us%65r:p%61ss@EXAMPLE.COM:00080/a/./b/../%7e?q=%41#%62');
    /// var_dump($r->toRawString(), $r->getRawHost(), $r->getRawPath());
    /// var_dump($r->getHost(), $r->getPath(), $r->getQuery(), $r->getUserInfo());
    /// var_dump($r->toRawString(), $r->toString(), (clone $r)->toString());
    /// $base = new R('http://a/b/c/d;p?q');
    /// foreach (['g', '../g', '?y', '#s', '//g', '', '../../../g', '/./g', 'http:g'] as $text) {
    ///     var_dump($base->resolve($text)->toRawString());
    /// }
    /// var_dump(R::parse('x', new R('relative/base')));
    /// foreach (['x:', 'x://', 'http://@h:?#', 'http://h', 'http://h/?#', 'http://[v1.a]/'] as $text) {
    ///     $u = new R($text);
    ///     var_dump($u->getRawHost(), $u->getRawUsername(), $u->getRawPassword(), $u->getPort(), $u->getRawQuery(), $u->getRawFragment());
    /// }
    /// ```
    /// ```expect
    /// string(61) "HTTP://Us%65r:p%61ss@EXAMPLE.COM:00080/a/./b/../%7e?q=%41#%62"
    /// string(11) "EXAMPLE.COM"
    /// string(13) "/a/./b/../%7e"
    /// string(11) "example.com"
    /// string(4) "/a/~"
    /// string(3) "q=A"
    /// string(9) "User:pass"
    /// string(61) "HTTP://Us%65r:p%61ss@EXAMPLE.COM:00080/a/./b/../%7e?q=%41#%62"
    /// string(41) "http://User:pass@example.com:80/a/~?q=A#b"
    /// string(41) "http://User:pass@example.com:80/a/~?q=A#b"
    /// string(14) "http://a/b/c/g"
    /// string(12) "http://a/b/g"
    /// string(18) "http://a/b/c/d;p?y"
    /// string(20) "http://a/b/c/d;p?q#s"
    /// string(8) "http://g"
    /// string(18) "http://a/b/c/d;p?q"
    /// string(10) "http://a/g"
    /// string(10) "http://a/g"
    /// string(6) "http:g"
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// string(0) ""
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// string(1) "h"
    /// string(0) ""
    /// string(0) ""
    /// NULL
    /// string(0) ""
    /// string(0) ""
    /// string(1) "h"
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// string(1) "h"
    /// NULL
    /// NULL
    /// NULL
    /// string(0) ""
    /// string(0) ""
    /// string(6) "[v1.a]"
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// NULL
    /// ```
    ///
    /// What the request's heap counts (design §5.13): the parse on
    /// construction, the normalized copy on the first normalized read and
    /// not on the second, a clone without that copy, an update, and what
    /// the three URIs' release gives back, 32 bytes short of their sum.
    ///
    /// ```php
    /// use Uri\Rfc3986\Uri as R;
    /// $warm = new R('https://example.com/a?b#c'); $warm->getHost();
    /// $m = memory_get_usage(); $u = new R('https://example.com/a?b#c'); echo 'raw ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $u->getRawHost(); echo 'raw-read ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $u->getHost(); echo 'normalize ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $u->getHost(); echo 'again ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $copy = clone $u; echo 'clone ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $copy->getHost(); echo 'clone-normalize ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $v = $u->withPath('/x/y'); echo 'update ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); unset($u, $copy, $v); echo 'free ', memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// raw 608
    /// raw-read 0
    /// normalize 112
    /// again 0
    /// clone 576
    /// clone-normalize 112
    /// update 624
    /// free -2000
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        uri: PhpString,
        #[php(default = null)] baseUrl: Option<Instance<Uri>>,
    ) -> Result<(), Thrown> {
        crate::construct::<R>(req, this.object(), uri, baseUrl.map(|b| b.object()), None)
    }

    /// `getScheme(): ?string`, normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("HTTPS://example.com"))->getScheme(), (new Uri\Rfc3986\Uri("/a"))->getScheme());
    /// ```
    /// ```expect
    /// string(5) "https"
    /// NULL
    /// ```
    fn getScheme(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "scheme",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.scheme(ReadAs::NormalizedAscii)),
        )
    }

    /// `getRawScheme(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("HTTPS://example.com"))->getRawScheme());
    /// ```
    /// ```expect
    /// string(5) "HTTPS"
    /// ```
    fn getRawScheme(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "scheme", ReadAs::Raw, |_, u| {
            Ok(u.scheme(ReadAs::Raw))
        })
    }

    /// `withScheme(?string $scheme): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com");
    /// var_dump($u->withScheme("ftp")->toString(), $u->withScheme(null)->toString(), $u->toString());
    /// try { $u->withScheme("1ab"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(17) "ftp://example.com"
    /// string(13) "//example.com"
    /// string(19) "https://example.com"
    /// The specified scheme is malformed
    /// ```
    fn withScheme(
        req: &mut Request,
        this: This<Self>,
        scheme: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<R>(req, this.object(), scheme, R::set_scheme)
    }

    /// `getUserInfo(): ?string`, normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://%75ser:p%61ss@example.com"))->getUserInfo(), (new Uri\Rfc3986\Uri("https://example.com"))->getUserInfo());
    /// ```
    /// ```expect
    /// string(9) "user:pass"
    /// NULL
    /// ```
    fn getUserInfo(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        user_info(req, this, ReadAs::NormalizedAscii)
    }

    /// `getRawUserInfo(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://%75ser:p%61ss@example.com"))->getRawUserInfo());
    /// ```
    /// ```expect
    /// string(13) "%75ser:p%61ss"
    /// ```
    fn getRawUserInfo(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        user_info(req, this, ReadAs::Raw)
    }

    /// `withUserInfo(#[\SensitiveParameter] ?string $userinfo): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com");
    /// var_dump($u->withUserInfo("a:b")->toString(), $u->withUserInfo("a:b")->withUserInfo(null)->toString());
    /// try { (new Uri\Rfc3986\Uri("/p"))->withUserInfo("a"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// try { $u->withUserInfo("a@b"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(23) "https://a:b@example.com"
    /// string(19) "https://example.com"
    /// Cannot set a userinfo without having a host
    /// The specified userinfo is malformed
    /// ```
    fn withUserInfo(
        req: &mut Request,
        this: This<Self>,
        #[php(sensitive)] userinfo: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<R>(req, this.object(), userinfo, R::set_user_info)
    }

    /// `getUsername(): ?string`, normalized: the user info up to its `:`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://%75ser:pass@example.com"))->getUsername(), (new Uri\Rfc3986\Uri("https://:pass@example.com"))->getUsername());
    /// ```
    /// ```expect
    /// string(4) "user"
    /// string(0) ""
    /// ```
    fn getUsername(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "username",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.username(ReadAs::NormalizedAscii)),
        )
    }

    /// `getRawUsername(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://%75ser:pass@example.com"))->getRawUsername());
    /// ```
    /// ```expect
    /// string(6) "%75ser"
    /// ```
    fn getRawUsername(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "username", ReadAs::Raw, |_, u| {
            Ok(u.username(ReadAs::Raw))
        })
    }

    /// `getPassword(): ?string`, normalized: the user info after its `:`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://user:p%61ss@example.com"))->getPassword(), (new Uri\Rfc3986\Uri("https://user@example.com"))->getPassword());
    /// ```
    /// ```expect
    /// string(4) "pass"
    /// string(0) ""
    /// ```
    fn getPassword(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "password",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.password(ReadAs::NormalizedAscii)),
        )
    }

    /// `getRawPassword(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://user:p%61ss@example.com"))->getRawPassword());
    /// ```
    /// ```expect
    /// string(6) "p%61ss"
    /// ```
    fn getRawPassword(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "password", ReadAs::Raw, |_, u| {
            Ok(u.password(ReadAs::Raw))
        })
    }

    /// `getHost(): ?string`, normalized; an IP address in its brackets.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://EXAMPLE.com"))->getHost(), (new Uri\Rfc3986\Uri("https://[::1]"))->getHost(), (new Uri\Rfc3986\Uri("/a"))->getHost());
    /// ```
    /// ```expect
    /// string(11) "example.com"
    /// string(5) "[::1]"
    /// NULL
    /// ```
    fn getHost(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "host",
            ReadAs::NormalizedAscii,
            |req, u| u.host(req, ReadAs::NormalizedAscii),
        )
    }

    /// `getRawHost(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://EXAMPLE.com"))->getRawHost());
    /// ```
    /// ```expect
    /// string(11) "EXAMPLE.com"
    /// ```
    fn getRawHost(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "host", ReadAs::Raw, |req, u| {
            u.host(req, ReadAs::Raw)
        })
    }

    /// `withHost(?string $host): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com/a");
    /// var_dump($u->withHost("[::1]")->toString(), $u->withHost(null)->toString());
    /// try { $u->withHost("a b"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// try { (new Uri\Rfc3986\Uri("https://example.com:80"))->withHost(null); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// try { (new Uri\Rfc3986\Uri("https://u@example.com"))->withHost(null); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(51) "https://[0000:0000:0000:0000:0000:0000:0000:0001]/a"
    /// string(8) "https:/a"
    /// The specified host is malformed
    /// Cannot remove the host from a URI that has a port
    /// Cannot remove the host from a URI that has a userinfo
    /// ```
    ///
    /// A refused update changes nothing: the URI a `with*()` threw on
    /// reads as it did, and an empty query is not an absent one.
    ///
    /// ```php
    /// use Uri\Rfc3986\Uri as R;
    /// $r = new R('HTTP://EXAMPLE.com/a/./b');
    /// var_dump($r->toString());
    /// foreach (['withHost' => 'bad host', 'withScheme' => '1bad', 'withPath' => '/%zz'] as $method => $text) {
    ///     try { $r->$method($text); } catch (Throwable $e) { var_dump($e->getMessage()); }
    ///     var_dump($r->toRawString(), $r->toString());
    /// }
    /// var_dump($r->withQuery('')->getRawQuery(), $r->withQuery(null)->getRawQuery());
    /// ```
    /// ```expect
    /// string(22) "http://example.com/a/b"
    /// string(31) "The specified host is malformed"
    /// string(24) "HTTP://EXAMPLE.com/a/./b"
    /// string(22) "http://example.com/a/b"
    /// string(33) "The specified scheme is malformed"
    /// string(24) "HTTP://EXAMPLE.com/a/./b"
    /// string(22) "http://example.com/a/b"
    /// string(31) "The specified path is malformed"
    /// string(24) "HTTP://EXAMPLE.com/a/./b"
    /// string(22) "http://example.com/a/b"
    /// string(0) ""
    /// NULL
    /// ```
    fn withHost(
        req: &mut Request,
        this: This<Self>,
        host: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<R>(req, this.object(), host, R::set_host)
    }

    /// `getPort(): ?int`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com:0443"))->getPort(), (new Uri\Rfc3986\Uri("https://example.com:"))->getPort());
    /// ```
    /// ```expect
    /// int(443)
    /// NULL
    /// ```
    fn getPort(req: &mut Request, this: This<Self>) -> Result<Option<i64>, Thrown> {
        crate::component::<R, _>(req, this.object(), "port", ReadAs::Raw, |_, u| {
            Ok(u.port(ReadAs::Raw))
        })
    }

    /// `withPort(?int $port): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com");
    /// var_dump($u->withPort(8080)->toString(), $u->withPort(8080)->withPort(null)->toString());
    /// try { $u->withPort(-1); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// try { (new Uri\Rfc3986\Uri("/a"))->withPort(1); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(24) "https://example.com:8080"
    /// string(19) "https://example.com"
    /// The specified port is malformed
    /// Cannot set a port without having a host
    /// ```
    fn withPort(
        req: &mut Request,
        this: This<Self>,
        port: Option<i64>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write::<R>(req, this.object(), |req, uri| uri.set_port(req, port))
    }

    /// `getPath(): string`, normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com/a/./b/../%7Ec"))->getPath(), (new Uri\Rfc3986\Uri("https://example.com"))->getPath(), (new Uri\Rfc3986\Uri("a/b"))->getPath());
    /// ```
    /// ```expect
    /// string(5) "/a/~c"
    /// string(0) ""
    /// string(3) "a/b"
    /// ```
    fn getPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        path(req, this, ReadAs::NormalizedAscii)
    }

    /// `getRawPath(): string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com/a/./b/../%7Ec"))->getRawPath(), (new Uri\Rfc3986\Uri("/"))->getRawPath());
    /// ```
    /// ```expect
    /// string(14) "/a/./b/../%7Ec"
    /// string(1) "/"
    /// ```
    fn getRawPath(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        path(req, this, ReadAs::Raw)
    }

    /// `withPath(string $path): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com/a");
    /// var_dump($u->withPath("/b/c")->toString(), $u->withPath("")->toString());
    /// try { $u->withPath("b"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(23) "https://example.com/b/c"
    /// string(19) "https://example.com"
    /// The specified path is malformed
    /// ```
    fn withPath(
        req: &mut Request,
        this: This<Self>,
        path: PhpString,
    ) -> Result<Owner<Static>, Thrown> {
        let path = path.as_bytes().to_vec();
        crate::write::<R>(req, this.object(), |req, uri| uri.set_path(req, &path))
    }

    /// `getQuery(): ?string`, normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com?a=%7e"))->getQuery(), (new Uri\Rfc3986\Uri("https://example.com"))->getQuery());
    /// ```
    /// ```expect
    /// string(3) "a=~"
    /// NULL
    /// ```
    fn getQuery(req: &mut Request, this: This<Self>) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "query",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.query(ReadAs::NormalizedAscii)),
        )
    }

    /// `getRawQuery(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com?a=%7e"))->getRawQuery());
    /// ```
    /// ```expect
    /// string(5) "a=%7e"
    /// ```
    fn getRawQuery(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "query", ReadAs::Raw, |_, u| {
            Ok(u.query(ReadAs::Raw))
        })
    }

    /// `withQuery(?string $query): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com");
    /// var_dump($u->withQuery("a=b")->toString(), $u->withQuery("a=b")->withQuery(null)->toString());
    /// try { $u->withQuery("a b"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(23) "https://example.com?a=b"
    /// string(19) "https://example.com"
    /// The specified query is malformed
    /// ```
    fn withQuery(
        req: &mut Request,
        this: This<Self>,
        query: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<R>(req, this.object(), query, R::set_query)
    }

    /// `getFragment(): ?string`, normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com#%7e"))->getFragment(), (new Uri\Rfc3986\Uri("https://example.com"))->getFragment());
    /// ```
    /// ```expect
    /// string(1) "~"
    /// NULL
    /// ```
    fn getFragment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(
            req,
            this.object(),
            "fragment",
            ReadAs::NormalizedAscii,
            |_, u| Ok(u.fragment(ReadAs::NormalizedAscii)),
        )
    }

    /// `getRawFragment(): ?string`.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("https://example.com#%7e"))->getRawFragment());
    /// ```
    /// ```expect
    /// string(3) "%7e"
    /// ```
    fn getRawFragment(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        crate::component_string::<R>(req, this.object(), "fragment", ReadAs::Raw, |_, u| {
            Ok(u.fragment(ReadAs::Raw))
        })
    }

    /// `withFragment(?string $fragment): static`.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com");
    /// var_dump($u->withFragment("f")->toString(), $u->withFragment("f")->withFragment(null)->toString());
    /// try { $u->withFragment("a b"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(21) "https://example.com#f"
    /// string(19) "https://example.com"
    /// The specified fragment is malformed
    /// ```
    fn withFragment(
        req: &mut Request,
        this: This<Self>,
        fragment: Option<PhpString>,
    ) -> Result<Owner<Static>, Thrown> {
        crate::write_bytes::<R>(req, this.object(), fragment, R::set_fragment)
    }

    /// `equals(Uri\Rfc3986\Uri $uri, Uri\UriComparisonMode
    /// $comparisonMode = Uri\UriComparisonMode::ExcludeFragment): bool`:
    /// the two normalized.
    ///
    /// ```php
    /// $a = new Uri\Rfc3986\Uri("HTTPS://example.com/%7e#x");
    /// $b = new Uri\Rfc3986\Uri("https://EXAMPLE.com/~#y");
    /// var_dump($a->equals($b), $a->equals($b, Uri\UriComparisonMode::IncludeFragment));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// ```
    fn equals(
        req: &mut Request,
        this: This<Self>,
        uri: Instance<Uri>,
        #[php(default = Uri::UriComparisonMode::ExcludeFragment)] comparisonMode: UriComparisonMode,
    ) -> Result<bool, Thrown> {
        crate::equals::<R>(req, this.object(), uri.object(), comparisonMode)
    }

    /// `toString(): string`: the URI normalized.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("HTTPS://EXAMPLE.com/a/../%7eb?%7e#%7e"))->toString());
    /// ```
    /// ```expect
    /// string(26) "https://example.com/~b?~#~"
    /// ```
    fn toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        crate::recompose::<R>(req, this.object(), Recomposition::NormalizedAscii)
    }

    /// `toRawString(): string`: the URI as parsed or written.
    ///
    /// ```php
    /// var_dump((new Uri\Rfc3986\Uri("HTTPS://EXAMPLE.com/a/../%7eb"))->toRawString());
    /// ```
    /// ```expect
    /// string(29) "HTTPS://EXAMPLE.com/a/../%7eb"
    /// ```
    fn toRawString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        crate::recompose::<R>(req, this.object(), Recomposition::RawAscii)
    }

    /// `resolve(string $uri): static`: a reference resolved against this
    /// URI, strictly.
    ///
    /// ```php
    /// $u = new Uri\Rfc3986\Uri("https://example.com/a/b?q");
    /// var_dump($u->resolve("../c")->toString(), $u->resolve("?r")->toString(), $u->resolve("//other.org")->toString());
    /// try { $u->resolve("exa mple"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(21) "https://example.com/c"
    /// string(25) "https://example.com/a/b?r"
    /// string(17) "https://other.org"
    /// The specified URI is malformed
    /// ```
    fn resolve(
        req: &mut Request,
        this: This<Self>,
        uri: PhpString,
    ) -> Result<Owner<Static>, Thrown> {
        crate::resolve::<R>(req, this.object(), uri, None)
    }

    /// `__serialize(): array`: `[["uri" => raw string], []]`.
    ///
    /// ```php
    /// $s = serialize(new Uri\Rfc3986\Uri("HTTPS://example.com/%7e"));
    /// var_dump($s, unserialize($s)->toRawString());
    /// ```
    /// ```expect
    /// string(88) "O:15:"Uri\Rfc3986\Uri":2:{i:0;a:1:{s:3:"uri";s:23:"HTTPS://example.com/%7e";}i:1;a:0:{}}"
    /// string(23) "HTTPS://example.com/%7e"
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        crate::serialize::<R>(req, this.object())
    }

    /// `__unserialize(array $data): void`: only the shape `__serialize()`
    /// writes, into an object not yet set.
    ///
    /// ```php
    /// foreach (['O:15:"Uri\Rfc3986\Uri":2:{i:0;a:1:{s:3:"uri";s:3:"a b";}i:1;a:0:{}}', 'O:15:"Uri\Rfc3986\Uri":2:{i:0;a:1:{s:3:"uri";s:1:"a";}i:1;a:1:{i:0;i:1;}}', 'O:15:"Uri\Rfc3986\Uri":1:{i:0;a:1:{s:3:"uri";s:1:"a";}}'] as $s) {
    ///     try { unserialize($s); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Invalid serialization data for Uri\Rfc3986\Uri object
    /// Invalid serialization data for Uri\Rfc3986\Uri object
    /// Invalid serialization data for Uri\Rfc3986\Uri object
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        crate::unserialize::<R>(req, this.object(), data)
    }

    /// `__debugInfo(): array`: each component read raw.
    ///
    /// ```php
    /// var_dump(new Uri\Rfc3986\Uri("HTTPS://u:p@EXAMPLE.com:81/a?b#c"));
    /// ```
    /// ```expect
    /// object(Uri\Rfc3986\Uri)#1 (8) {
    ///   ["scheme"]=>
    ///   string(5) "HTTPS"
    ///   ["username"]=>
    ///   string(1) "u"
    ///   ["password"]=>
    ///   string(1) "p"
    ///   ["host"]=>
    ///   string(11) "EXAMPLE.com"
    ///   ["port"]=>
    ///   int(81)
    ///   ["path"]=>
    ///   string(2) "/a"
    ///   ["query"]=>
    ///   string(1) "b"
    ///   ["fragment"]=>
    ///   string(1) "c"
    /// }
    /// ```
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        crate::debug_info::<R>(req, this.object())
    }
}

/// `rfc3986_userinfo_read`: a user info the parser cannot read is a
/// plain `Error` here, not the `UriError` of `php_uri_property_read_helper`.
fn user_info(
    req: &mut Request,
    this: This<Uri>,
    mode: ReadAs,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let bytes = crate::read::<R, _>(req, this.object(), mode, |_, u| Ok(u.user_info(mode)))?;
    if let Ok(bytes) = bytes {
        bytes.map(|bytes| PhpString::new(req, &bytes)).transpose()
    } else {
        let ce = Throwables::of(req)?;
        Err(throw_error(
            req,
            ce.error,
            "The userinfo component cannot be retrieved",
        ))
    }
}

/// The path read, a string always.
fn path(req: &mut Request, this: This<Uri>, mode: ReadAs) -> Result<Owner<PhpString>, Thrown> {
    let bytes =
        crate::component::<R, _>(req, this.object(), "path", mode, |_, u| Ok(u.path(mode)))?;
    PhpString::new(req, &bytes)
}

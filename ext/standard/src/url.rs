//! `Z/ext/standard/url.c`: `parse_url` over `php_url_parse_ex2`, and
//! the four codecs over `php_url_encode_impl`, `php_url_decode_ex` and
//! `php_raw_url_decode_ex`, which `http_build_query` and `parse_str`
//! read too; and `get_headers` over the `wrapper_data` a URL wrapper's
//! stream carries.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Path, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::{Request, StreamCtx, Streams as _};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Resource, Value, ValueRef};
use php_ext_uri::{Parser, ReadAs, UriStruct};

use crate::streamsfuncs::{NullContext, context_of};

pub(super) use constants::PHP_QUERY_RFC3986;
use constants::{
    PHP_URL_FRAGMENT, PHP_URL_HOST, PHP_URL_PASS, PHP_URL_PATH, PHP_URL_PORT, PHP_URL_QUERY,
    PHP_URL_SCHEME, PHP_URL_USER,
};

/// The constants `url.c`'s natives read, in the stub's order:
/// `PHP_URL_*`, `parse_url`'s component selectors, and `PHP_QUERY_*`,
/// `http_build_query`'s encodings.
///
/// ```php
/// var_dump(PHP_URL_SCHEME, PHP_URL_HOST, PHP_URL_PORT, PHP_URL_USER, PHP_URL_PASS, PHP_URL_PATH, PHP_URL_QUERY, PHP_URL_FRAGMENT, PHP_QUERY_RFC1738, PHP_QUERY_RFC3986);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(5)
/// int(6)
/// int(7)
/// int(1)
/// int(2)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const PHP_URL_SCHEME: i64 = 0;
    pub(crate) const PHP_URL_HOST: i64 = 1;
    pub(crate) const PHP_URL_PORT: i64 = 2;
    pub(crate) const PHP_URL_USER: i64 = 3;
    pub(crate) const PHP_URL_PASS: i64 = 4;
    pub(crate) const PHP_URL_PATH: i64 = 5;
    pub(crate) const PHP_URL_QUERY: i64 = 6;
    pub(crate) const PHP_URL_FRAGMENT: i64 = 7;
    pub(crate) const PHP_QUERY_RFC1738: i64 = 1;
    pub(crate) const PHP_QUERY_RFC3986: i64 = 2;
}

/// `get_headers(string $url, bool $associative = false, $context = null):
/// array|false`, php's `PHP_FUNCTION(get_headers)`: the URL opened `r`
/// under `STREAM_USE_URL | STREAM_ONLY_GET_HEADERS` ([`Request::open_url`])
/// within the `context` given or the default one, and the header lines its
/// wrapper left in the stream's `wrapper_data`, each as a string, or with
/// `$associative` keyed by the name before its first `:` with the blanks
/// after it skipped, a repeated name a list, and a line with no `:` under
/// the next integer key; `false` where nothing opened or the wrapper left
/// no array, as a user-space wrapper's object is. A path whose wrapper is
/// no URL wrapper is php's warning. The http wrapper keeps the stream of a
/// response that is no success, since only the head is asked.
///
/// ```php
/// class W { public $context; function stream_open($p, $m, $o, &$op) { return true; } function stream_read($n) { return ""; } function stream_eof() { return true; } }
/// stream_wrapper_register("web", "W", STREAM_IS_URL);
/// var_dump(get_headers("web://x"), get_headers("web://x", true, null));
/// try { get_headers("web://x", false, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { get_headers("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// get_headers(): Argument #3 ($context) must be of type resource or null, int given
/// get_headers(): Argument #1 ($url) must not contain any null bytes
/// ```
///
/// ```php
/// $srv = stream_socket_server('tcp://127.0.0.1:0');
/// $addr = stream_socket_get_name($srv, false);
/// if (pcntl_fork() === 0) {
///     $c = stream_socket_accept($srv);
///     $asked = str_contains(fread($c, 1024), "X-Asked: yes") ? 'yes' : 'no';
///     fwrite($c, "HTTP/1.0 404 Not Found\r\nX-A: 1\r\nX-A: 2\r\nX-Got: $asked\r\n\r\nbody");
///     fclose($c);
///     exit(0);
/// }
/// $ctx = stream_context_create(['http' => ['header' => 'X-Asked: yes']]);
/// var_dump(get_headers("http://$addr/", true, $ctx));
/// pcntl_wait($status);
/// ```
/// ```expect process
/// array(3) {
///   [0]=>
///   string(22) "HTTP/1.0 404 Not Found"
///   ["X-A"]=>
///   array(2) {
///     [0]=>
///     string(1) "1"
///     [1]=>
///     string(1) "2"
///   }
///   ["X-Got"]=>
///   string(3) "yes"
/// }
/// ```
#[php_function]
fn get_headers(
    req: &mut Request,
    url: Path,
    #[php(default = false)] associative: bool,
    #[php(default = null)] context: Option<Resource>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let context = context_of(req, context, NullContext::Default)?;
    let Some(stream) = req.with_stream_ctx(context, |req| req.open_url(url.0.as_bytes(), b"r"))?
    else {
        return Ok(OrFalse::False);
    };
    let meta = stream.meta_data(req)?;
    let lines = meta
        .find(b"wrapper_data")
        .and_then(|value| value.deref().as_array().copied());
    let Some(lines) = lines else {
        meta.give_up(req);
        stream.hand_on_unmigrated().free(req);
        return Ok(OrFalse::False);
    };
    let mut headers = PhpArray::new(req)?;
    for (_, line) in lines.snapshot() {
        let line = line.deref();
        let ValueRef::Str(line) = line.as_ref() else {
            continue;
        };
        add_header(req, &mut headers, line.as_bytes(), associative)?;
    }
    meta.give_up(req);
    stream.hand_on_unmigrated().free(req);
    Ok(OrFalse::Value(headers))
}

/// One header line into `get_headers`' answer: appended as it is, or
/// with `associative` and a `:` in it, under the name before the `:`
/// with the blanks after it skipped. A name met again makes the first
/// value a list, `convert_to_array`, and the rest are appended to it.
fn add_header(
    req: &mut Request,
    headers: &mut Owner<PhpArray>,
    line: &[u8],
    associative: bool,
) -> Result<(), Thrown> {
    let Some(at) = line.iter().position(|c| *c == b':').filter(|_| associative) else {
        let value = Value::string(PhpString::new(req, line)?.hand_on_unmigrated());
        return headers.append(req, value);
    };
    let mut start = at + 1;
    while line.get(start).is_some_and(u8::is_ascii_whitespace) {
        start += 1;
    }
    let value = Value::string(PhpString::new(req, &line[start..])?.hand_on_unmigrated());
    let Some(previous) = headers.find(&line[..at]) else {
        let name = Key::Str(PhpString::new(req, &line[..at])?.hand_on_unmigrated());
        return headers.set(req, name, value);
    };
    if let Some(list) = previous.as_array() {
        let mut list = *list;
        return list.append(req, value);
    }
    let mut list = PhpArray::new(req)?;
    req.addref(&previous);
    list.append(req, previous)?;
    list.append(req, value)?;
    let name = Key::Str(PhpString::new(req, &line[..at])?.hand_on_unmigrated());
    headers.set(req, name, Value::array(list.hand_on_unmigrated()))
}

/// `php_url`: the components `php_url_parse_ex2` found.
#[derive(Default)]
pub(super) struct Url {
    pub(super) scheme: Option<Vec<u8>>,
    pub(super) user: Option<Vec<u8>>,
    pub(super) pass: Option<Vec<u8>>,
    pub(super) host: Option<Vec<u8>>,
    pub(super) port: Option<u16>,
    pub(super) path: Option<Vec<u8>>,
    pub(super) query: Option<Vec<u8>>,
    pub(super) fragment: Option<Vec<u8>>,
}

/// `php_replace_controlchars`: every control byte as `_`.
fn clean(bytes: &[u8]) -> Vec<u8> {
    bytes
        .iter()
        .map(|&c| if c < 32 || c == 127 { b'_' } else { c })
        .collect()
}

/// `binary_strcspn(s, e, chars)`: the first offset in `from..to` of
/// any of `chars`, or `to`.
fn strcspn(s: &[u8], from: usize, to: usize, chars: &[u8]) -> usize {
    s[from..to]
        .iter()
        .position(|c| chars.contains(c))
        .map_or(to, |at| at + from)
}

/// `ZEND_STRTOL(port_buf, &end, 10)` over up to five digits: the value,
/// or `None` where no digit was read.
fn port_of(digits: &[u8]) -> Option<i64> {
    let n = digits.iter().take_while(|c| c.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    std::str::from_utf8(&digits[..n]).ok()?.parse().ok()
}

/// `php_url_parse_ex2(str, length, &has_port)`: `None` where php
/// refuses the string as a URL.
#[allow(clippy::too_many_lines)]
pub(super) fn parse(str: &[u8]) -> Option<Url> {
    // What the scheme branch hands on: where the host starts, a port
    // read after a scheme-less colon, or straight to the path.
    enum Next {
        Host,
        Port(usize),
        Path,
    }
    let mut ret = Url::default();
    let ue = str.len();
    let mut s = 0usize;
    let at = |i: usize| str.get(i).copied().unwrap_or(0);
    let colon = str.iter().position(|c| *c == b':');
    let next = match colon {
        Some(e) if e != s => {
            let valid = str[s..e]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'.' | b'-'));
            if !valid {
                if e + 1 < ue && e < strcspn(str, s, ue, b"?#") {
                    Next::Port(e)
                } else if s + 1 < ue && str[s] == b'/' && str[s + 1] == b'/' {
                    s += 2;
                    Next::Host
                } else {
                    Next::Path
                }
            } else if e + 1 == ue {
                ret.scheme = Some(clean(&str[s..e]));
                return Some(ret);
            } else if str[e + 1] != b'/' {
                let mut p = e + 1;
                while p < ue && str[p].is_ascii_digit() {
                    p += 1;
                }
                if (p == ue || str[p] == b'/') && (p - e) < 7 {
                    Next::Port(e)
                } else {
                    ret.scheme = Some(clean(&str[s..e]));
                    s = e + 1;
                    Next::Path
                }
            } else {
                ret.scheme = Some(clean(&str[s..e]));
                if e + 2 < ue && str[e + 2] == b'/' {
                    s = e + 3;
                    let file = ret
                        .scheme
                        .as_deref()
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case(b"file"));
                    if file && e + 3 < ue && str[e + 3] == b'/' {
                        if e + 5 < ue && str[e + 5] == b':' {
                            s = e + 4;
                        }
                        Next::Path
                    } else {
                        Next::Host
                    }
                } else {
                    s = e + 1;
                    Next::Path
                }
            }
        }
        Some(e) => Next::Port(e),
        None => {
            if s + 1 < ue && str[s] == b'/' && str[s + 1] == b'/' {
                s += 2;
                Next::Host
            } else {
                Next::Path
            }
        }
    };
    let next = match next {
        Next::Port(e) => {
            // `parse_port:`
            let p = e + 1;
            let mut pp = p;
            while pp < ue && pp - p < 6 && str[pp].is_ascii_digit() {
                pp += 1;
            }
            if pp - p > 0 && pp - p < 6 && (pp == ue || str[pp] == b'/') {
                match port_of(&str[p..pp]) {
                    Some(port) if (0..=65535).contains(&port) => {
                        ret.port = u16::try_from(port).ok();
                        if s + 1 < ue && str[s] == b'/' && str[s + 1] == b'/' {
                            s += 2;
                        }
                        Next::Host
                    }
                    _ => return None,
                }
            } else if p == pp && pp == ue {
                return None;
            } else if s + 1 < ue && str[s] == b'/' && str[s + 1] == b'/' {
                s += 2;
                Next::Host
            } else {
                Next::Path
            }
        }
        other => other,
    };
    if matches!(next, Next::Host) {
        // `parse_host:`
        let e = strcspn(str, s, ue, b"/?#");
        if let Some(p) = str[s..e].iter().rposition(|c| *c == b'@').map(|at| at + s) {
            match str[s..p].iter().position(|c| *c == b':').map(|at| at + s) {
                Some(pp) => {
                    ret.user = Some(clean(&str[s..pp]));
                    ret.pass = Some(clean(&str[pp + 1..p]));
                }
                None => ret.user = Some(clean(&str[s..p])),
            }
            s = p + 1;
        }
        let ipv6 = s < ue && str[s] == b'[' && e > 0 && at(e - 1) == b']';
        let colon = if ipv6 || s > e {
            None
        } else {
            str[s..e].iter().rposition(|c| *c == b':').map(|at| at + s)
        };
        let host_end = match colon {
            Some(p) => {
                if ret.port.is_none_or(|port| port == 0) {
                    let digits = &str[p + 1..e];
                    if digits.len() > 5 {
                        return None;
                    } else if !digits.is_empty() {
                        match port_of(digits) {
                            Some(port) if (0..=65535).contains(&port) => {
                                ret.port = u16::try_from(port).ok();
                            }
                            _ => return None,
                        }
                    }
                }
                p
            }
            None => e,
        };
        if host_end < s + 1 {
            return None;
        }
        ret.host = Some(clean(&str[s..host_end]));
        if e == ue {
            return Some(ret);
        }
        s = e;
    }
    // `just_path:`
    let mut e = ue;
    if let Some(p) = str[s..e].iter().position(|c| *c == b'#').map(|at| at + s) {
        ret.fragment = Some(clean(&str[p + 1..e]));
        e = p;
    }
    if let Some(p) = str[s..e].iter().position(|c| *c == b'?').map(|at| at + s) {
        ret.query = Some(clean(&str[p + 1..e]));
        e = p;
    }
    if s < e || s == ue {
        ret.path = Some(clean(&str[s..e]));
    }
    Some(ret)
}

/// One row of `parse_url`'s answer: the selector, the key, the bytes.
type Part<'a> = (i64, &'a [u8], Option<&'a Vec<u8>>);

/// `parse_url(string $url, int $component = -1):
/// int|array|string|null|false`: the components as an array, in
/// `scheme`, `host`, `port`, `user`, `pass`, `path`, `query`,
/// `fragment` order and only those present, or the one `$component`
/// names, `null` where it is absent; `false` for a string php does not
/// read as a URL.
///
/// ```php
/// var_dump(parse_url("https://user:pw@example.com:8080/p/a/t/h?query=1#frag"), parse_url("//example.com/x"), parse_url("/just/path?q"), parse_url("mailto:me@example.com"), parse_url("example.com:80"), parse_url("http://[::1]:80/"), parse_url("http:///x"), parse_url("http://a:99999"), parse_url("file:///etc/passwd"), parse_url("x:"), parse_url("a#"), parse_url("http://h\x01st/"));
/// var_dump(parse_url("http://x.com:81/y", PHP_URL_PORT), parse_url("http://x.com/y", PHP_URL_PORT), parse_url("http://x.com/y", PHP_URL_HOST), parse_url("//x", PHP_URL_SCHEME));
/// try { parse_url("x", 8); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(8) {
///   ["scheme"]=>
///   string(5) "https"
///   ["host"]=>
///   string(11) "example.com"
///   ["port"]=>
///   int(8080)
///   ["user"]=>
///   string(4) "user"
///   ["pass"]=>
///   string(2) "pw"
///   ["path"]=>
///   string(8) "/p/a/t/h"
///   ["query"]=>
///   string(7) "query=1"
///   ["fragment"]=>
///   string(4) "frag"
/// }
/// array(2) {
///   ["host"]=>
///   string(11) "example.com"
///   ["path"]=>
///   string(2) "/x"
/// }
/// array(2) {
///   ["path"]=>
///   string(10) "/just/path"
///   ["query"]=>
///   string(1) "q"
/// }
/// array(2) {
///   ["scheme"]=>
///   string(6) "mailto"
///   ["path"]=>
///   string(14) "me@example.com"
/// }
/// array(2) {
///   ["host"]=>
///   string(11) "example.com"
///   ["port"]=>
///   int(80)
/// }
/// array(4) {
///   ["scheme"]=>
///   string(4) "http"
///   ["host"]=>
///   string(5) "[::1]"
///   ["port"]=>
///   int(80)
///   ["path"]=>
///   string(1) "/"
/// }
/// bool(false)
/// bool(false)
/// array(2) {
///   ["scheme"]=>
///   string(4) "file"
///   ["path"]=>
///   string(11) "/etc/passwd"
/// }
/// array(1) {
///   ["scheme"]=>
///   string(1) "x"
/// }
/// array(2) {
///   ["path"]=>
///   string(1) "a"
///   ["fragment"]=>
///   string(0) ""
/// }
/// array(3) {
///   ["scheme"]=>
///   string(4) "http"
///   ["host"]=>
///   string(4) "h_st"
///   ["path"]=>
///   string(1) "/"
/// }
/// int(81)
/// NULL
/// string(5) "x.com"
/// NULL
/// parse_url(): Argument #2 ($component) must be a valid URL component identifier, 8 given
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// foreach (['parse_url', 'urlencode', 'urldecode', 'rawurlencode', 'rawurldecode'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function parse_url ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $url ]
///     Parameter #1 [ <optional> int $component = -1 ]
///   }
///   - Return [ array|string|int|false|null ]
/// }
/// Function [ <internal:standard> function urlencode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function urldecode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function rawurlencode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function rawurldecode ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $string ]
///   }
///   - Return [ string ]
/// }
/// ```
#[php_function]
fn parse_url(
    req: &mut Request,
    url: PhpString,
    #[php(default = -1)] component: i64,
) -> Result<Option<OrFalse<Component>>, Thrown> {
    let key = component;
    let Some(url) = parse(url.as_bytes()) else {
        return Ok(Some(OrFalse::False));
    };
    let parts: [Part; 8] = [
        (PHP_URL_SCHEME, b"scheme", url.scheme.as_ref()),
        (PHP_URL_HOST, b"host", url.host.as_ref()),
        (PHP_URL_PORT, b"port", None),
        (PHP_URL_USER, b"user", url.user.as_ref()),
        (PHP_URL_PASS, b"pass", url.pass.as_ref()),
        (PHP_URL_PATH, b"path", url.path.as_ref()),
        (PHP_URL_QUERY, b"query", url.query.as_ref()),
        (PHP_URL_FRAGMENT, b"fragment", url.fragment.as_ref()),
    ];
    if key > -1 {
        if key == PHP_URL_PORT {
            return Ok(url
                .port
                .map(|port| OrFalse::Value(Union::A(i64::from(port)))));
        }
        return match parts.iter().find(|(id, _, _)| *id == key) {
            Some((_, _, Some(bytes))) => {
                let part = PhpString::new(req, bytes)?;
                Ok(Some(OrFalse::Value(Union::B(Union::B(part)))))
            }
            Some((_, _, None)) => Ok(None),
            None => Err(arg_value_error(
                req,
                2,
                &format!("be a valid URL component identifier, {key} given"),
            )),
        };
    }
    let mut array = PhpArray::new(req)?;
    for (id, name, bytes) in parts {
        let value = if id == PHP_URL_PORT {
            match url.port {
                Some(port) => Value::int(i64::from(port)),
                None => continue,
            }
        } else {
            match bytes {
                Some(bytes) => Value::string(PhpString::new(req, bytes)?.hand_on_unmigrated()),
                None => continue,
            }
        };
        let key = Key::Str(PhpString::new(req, name)?.hand_on_unmigrated());
        array.set(req, key, value)?;
    }
    Ok(Some(OrFalse::Value(Union::B(Union::A(array)))))
}

/// What `parse_url` answers short of `false` and `null`: the
/// `PHP_URL_PORT` number, every part as an array, or one part.
type Component = Union<i64, Union<Owner<PhpArray>, Owner<PhpString>>>;

/// `hexchars`.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `php_url_encode_impl(to, s, len, raw)`: every byte but the
/// alphanumerics and `-_.` as `%XX`, a space as `+` unless `raw`, and
/// `~` kept when `raw`.
pub(super) fn url_encode(s: &[u8], raw: bool, out: &mut Vec<u8>) {
    for &c in s {
        if !raw && c == b' ' {
            out.push(b'+');
        } else if (c < b'0' && c != b'-' && c != b'.')
            || (c < b'A' && c > b'9')
            || (c > b'Z' && c < b'a' && c != b'_')
            || (c > b'z' && (!raw || c != b'~'))
        {
            out.extend_from_slice(&[b'%', HEX[usize::from(c >> 4)], HEX[usize::from(c & 15)]]);
        } else {
            out.push(c);
        }
    }
}

/// `php_url_decode_ex` and, without `plus`, `php_raw_url_decode_ex`:
/// `%XX` with two hex digits as its byte, and `+` as a space unless
/// raw.
fn decode(s: &[u8], plus: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if plus && c == b'+' {
            out.push(b' ');
        } else if c == b'%'
            && i + 2 < s.len()
            && s[i + 1].is_ascii_hexdigit()
            && s[i + 2].is_ascii_hexdigit()
        {
            let hex = |x: u8| (x as char).to_digit(16).unwrap_or(0);
            out.push(u8::try_from(hex(s[i + 1]) * 16 + hex(s[i + 2])).unwrap_or(0));
            i += 2;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// `php_url_decode(str, len)`.
pub(super) fn php_url_decode(s: &[u8]) -> Vec<u8> {
    decode(s, true)
}

/// `php_raw_url_decode(str, len)`.
pub(super) fn php_raw_url_decode(s: &[u8]) -> Vec<u8> {
    decode(s, false)
}

/// The parser `php_stream_context_get_uri_parser` names for a wrapper,
/// decoded once: `parse_url`, the default, which this crate keeps, or a
/// class of ext/uri's (design §4.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UriParser {
    ParseUrl,
    Uri(Parser),
}

impl UriParser {
    /// `php_stream_context_get_uri_parser(wrapper, context)`
    /// (`Z/main/streams/streams.c:2490`): `parse_url` for no `context`
    /// and for a `uri_parser_class` option that is absent or `null`, the
    /// parser registered under the class a string names, and `None`,
    /// which the wrappers turn into php's `ValueError`, for a string that
    /// names none and for any other value.
    pub fn of_ctx(
        req: &mut Request,
        ctx: Option<StreamCtx>,
        wrapper: &[u8],
    ) -> Result<Option<UriParser>, Thrown> {
        let Some(ctx) = ctx else {
            return Ok(Some(UriParser::ParseUrl));
        };
        let named = ctx.option(req, wrapper, b"uri_parser_class");
        Ok(match named.as_ref().map(Value::as_ref) {
            None | Some(ValueRef::Null) => Some(UriParser::ParseUrl),
            Some(ValueRef::Str(name)) => match name.as_bytes() {
                b"parse_url" => Some(UriParser::ParseUrl),
                name => Parser::named(name).map(UriParser::Uri),
            },
            Some(_) => None,
        })
    }

    /// `php_uri_parse_to_struct(parser, text, PHP_URI_COMPONENT_READ_MODE_RAW,
    /// true)`: `None` where the parser refuses the text; `parse_url`'s
    /// `port` of zero is no `port`, as `uri_parser_php_parse_url.c` reads it.
    pub fn parse(self, req: &mut Request, text: &[u8]) -> Result<Option<UriStruct>, Thrown> {
        match self {
            UriParser::Uri(parser) => parser.parse_to_struct(req, text, ReadAs::Raw, true),
            UriParser::ParseUrl => Ok(parse(text).map(|url| UriStruct {
                scheme: url.scheme,
                user: url.user,
                password: url.pass,
                host: url.host,
                port: url.port.filter(|port| *port != 0).map(i64::from),
                path: url.path,
                query: url.query,
                fragment: url.fragment,
            })),
        }
    }
}

/// `urlencode(string $string): string`, the form encoding.
///
/// ```php
/// var_dump(strtr(urlencode("a b&c=d/é~_.-*"), "%", "#"), urlencode(""), strtr(urlencode("\x00\xff+"), "%", "#"));
/// ```
/// ```expect
/// string(29) "a+b#26c#3Dd#2F#C3#A9#7E_.-#2A"
/// string(0) ""
/// string(9) "#00#FF#2B"
/// ```
#[php_function]
fn urlencode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    encoded(req, string, false)
}

/// `rawurlencode(string $string): string`, RFC 3986's.
///
/// ```php
/// var_dump(strtr(rawurlencode("a b+c~d_e.f-g*h/"), "%", "#"), rawurlencode(""), strtr(rawurlencode("\x00\xff"), "%", "#"));
/// ```
/// ```expect
/// string(24) "a#20b#2Bc~d_e.f-g#2Ah#2F"
/// string(0) ""
/// string(6) "#00#FF"
/// ```
#[php_function]
fn rawurlencode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    encoded(req, string, true)
}

fn encoded(req: &mut Request, string: PhpString, raw: bool) -> Result<Owner<PhpString>, Thrown> {
    let mut out = Vec::with_capacity(string.len() * 3);
    url_encode(string.as_bytes(), raw, &mut out);
    PhpString::new(req, &out)
}

/// `urldecode(string $string): string`.
///
/// ```php
/// var_dump(urldecode("a+b%20c%zz%4%41"), bin2hex(urldecode("%E9%00")), urldecode("100%"));
/// ```
/// ```expect
/// string(11) "a b c%zz%4A"
/// string(4) "e900"
/// string(4) "100%"
/// ```
#[php_function]
fn urldecode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let out = decode(string.as_bytes(), true);
    PhpString::new(req, &out)
}

/// `rawurldecode(string $string): string`.
///
/// ```php
/// var_dump(rawurldecode("a+b%20c%41%4"), rawurldecode("%7e%7E"), rawurldecode("%"));
/// ```
/// ```expect
/// string(8) "a+b cA%4"
/// string(2) "~~"
/// string(1) "%"
/// ```
#[php_function]
fn rawurldecode(req: &mut Request, string: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let out = decode(string.as_bytes(), false);
    PhpString::new(req, &out)
}

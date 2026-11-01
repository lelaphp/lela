//! The `iconv` extension: `php-src/ext/iconv` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, `libc`, for the host's
//! `iconv(3)` in `ffi`; one entry, `extension()`; its state on the value
//! (ADR-0005). The phpt subset is `php-src/ext/iconv/tests`, through
//! `xtask phpt --subset`.
//!
//! `iconv.c`'s conversions, character counts, substrings and searches
//! over `UCS-4LE`, php's `GENERIC_SUPERSET_NAME`, one converter per
//! call; the MIME header codecs in `mime`; the `convert.iconv.*` stream
//! filter in `filter`; `ob_iconv_handler`; the three `iconv.*` ini
//! entries, which fall back on php's own `input_encoding`,
//! `internal_encoding` and `output_encoding` and then on
//! `default_charset`; and the four constants.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
mod filter;
mod mime;

use ffi::{Cd, Errno};
use php_ext::abi::{Extension, ExtensionBuilder, IniStage, OrFalse, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::{
    BufferHandler, ContentType, HandlerCall, HandlerStatus, Request, Streams as _,
};
use php_ext::values::{FreshString, Owner, PhpArray, PhpString};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// `iconv.stub.php`'s four constants, `ICONV_IMPL` and `ICONV_VERSION`
/// the `iconv(3)` linked, as `iconv.c` names it (`ffi::linked`): the
/// macOS system libiconv, 1.11, as the oracle's; and the same two the
/// rows of `--ri iconv` print, as `PHP_MINFO` reads them back.
///
/// ```php
/// var_dump(ICONV_IMPL, ICONV_VERSION);
/// ob_start(); (new ReflectionExtension('iconv'))->info(); $info = ob_get_clean();
/// var_dump(str_contains($info, "iconv implementation => " . ICONV_IMPL . "\n"), str_contains($info, "iconv library version => " . ICONV_VERSION . "\n"));
/// ```
/// ```expect
/// string(8) "libiconv"
/// string(4) "1.11"
/// bool(true)
/// bool(true)
/// ```
#[php_const]
mod constants {
    use crate::ffi::linked;

    /// `PHP_ICONV_IMPL_VALUE`, as `configure` named the library linked.
    pub(crate) const ICONV_IMPL: &str = linked::IMPL;
    /// `get_iconv_version()`, the library's own, read at startup.
    pub(crate) fn ICONV_VERSION() -> String {
        linked::version()
    }
    pub(crate) const ICONV_MIME_DECODE_STRICT: i64 = 1;
    pub(crate) const ICONV_MIME_DECODE_CONTINUE_ON_ERROR: i64 = 2;
}

/// `GENERIC_SUPERSET_NAME`, four bytes a character.
const UCS4: &[u8] = b"UCS-4LE";
/// `ICONV_CSNMAXLEN`.
const MAX_NAME: usize = 64;
/// The three encodings, in the order `iconv_get_encoding('all')` lists
/// them.
#[derive(Clone, Copy)]
#[rustfmt::skip]
enum Kind { Input, Output, Internal }

impl Kind {
    const ALL: [Kind; 3] = [Kind::Input, Kind::Output, Kind::Internal];

    /// php's own entry, and the `$type` the two natives take.
    fn name(self) -> &'static str {
        match self {
            Kind::Input => "input_encoding",
            Kind::Output => "output_encoding",
            Kind::Internal => "internal_encoding",
        }
    }

    /// The `iconv.*` entry.
    fn entry(self) -> String {
        format!("iconv.{}", self.name())
    }

    /// `$type`, as php's `strcasecmp` reads it.
    fn parse(given: &[u8]) -> Option<Kind> {
        let named = |kind: &Kind| kind.name().as_bytes().eq_ignore_ascii_case(given);
        Kind::ALL.into_iter().find(named)
    }
}

/// The extension value (design §7.1): the natives in the stub's order,
/// each declared by `#[php_function]`, the constants, and the state
/// declared on it.
///
/// ```php
/// var_dump(extension_loaded('iconv'), ICONV_IMPL, ICONV_VERSION, ICONV_MIME_DECODE_STRICT, ICONV_MIME_DECODE_CONTINUE_ON_ERROR, ini_get('iconv.internal_encoding'));
/// ```
/// ```expect
/// bool(true)
/// string(8) "libiconv"
/// string(4) "1.11"
/// int(1)
/// int(2)
/// string(0) ""
/// ```
///
/// Each declaration is the stub's, as reflection reads it.
///
/// ```php
/// echo new ReflectionFunction('iconv_substr'), new ReflectionFunction('iconv_get_encoding');
/// ```
/// ```expect
/// Function [ <internal:iconv> function iconv_substr ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <required> int $offset ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> ?string $encoding = null ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:iconv> function iconv_get_encoding ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> string $type = "all" ]
///   }
///   - Return [ array|string|false ]
/// }
/// ```
pub fn extension() -> Extension {
    let ini = |kind: Kind| IniDecl {
        name: Name(kind.entry().into_bytes().into()),
        default: Some(Box::new([])),
        user: true,
        per_dir: true,
        system: true,
    };
    ExtensionBuilder::default()
        .functions(vec![
            iconv_strlen::declared(),
            iconv_substr::declared(),
            iconv_strpos::declared(),
            iconv_strrpos::declared(),
            mime::iconv_mime_encode::declared(),
            mime::iconv_mime_decode::declared(),
            mime::iconv_mime_decode_headers::declared(),
            iconv::declared(),
            iconv_set_encoding::declared(),
            iconv_get_encoding::declared(),
        ])
        .constants(constants::declared())
        .ini(Kind::ALL.map(ini).to_vec())
        .on_modify(b"iconv.input_encoding", on_input_encoding)
        .on_modify(b"iconv.output_encoding", on_output_encoding)
        .on_modify(b"iconv.internal_encoding", on_internal_encoding)
        .register(BufferHandler {
            name: "ob_iconv_handler",
            conflicts: &["ob_iconv_handler", "mb_output_handler"],
            handler: ob_iconv_handler,
            init: None,
        })
        .info(info)
        .request_state::<State>()
        .rinit(rinit)
        .build("iconv")
}

/// `php_iconv_stream_filter_register_factory`, which php's `MINIT`
/// runs: the `convert.iconv.*` factory in the request's filter table,
/// the one `stream_filter_register` adds to, as ext/standard keeps its
/// string filters there.
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::rinit`'s signature
fn rinit(req: &mut Request) -> Result<(), Thrown> {
    req.register_filter(filter::NAME, filter::factory);
    Ok(())
}

/// `PHP_MINFO_FUNCTION(miconv)`: the table, then `DISPLAY_INI_ENTRIES`.
///
/// ```php
/// $e = new ReflectionExtension('iconv'); ob_start(); $e->info(); echo ob_get_clean();
/// ```
/// ```expect
///
/// iconv
///
/// iconv support => enabled
/// iconv implementation => libiconv
/// iconv library version => 1.11
///
/// Directive => Local Value => Master Value
/// iconv.input_encoding => no value => no value
/// iconv.output_encoding => no value => no value
/// iconv.internal_encoding => no value => no value
/// ```
fn info(req: &mut Request) {
    let version = ffi::linked::version();
    req.extensions_mut().info_table(&[
        &["iconv support", "enabled"],
        &["iconv implementation", ffi::linked::IMPL],
        &["iconv library version", &version],
    ]);
    let names = Kind::ALL.map(Kind::entry);
    req.ini_mut()
        .info_ini_entries(&names.each_ref().map(String::as_bytes));
}

/// `OnUpdateInputEncoding`, `OnUpdateOutputEncoding` and
/// `OnUpdateInternalEncoding`: a name of `ICONV_CSNMAXLEN` bytes or more
/// refused, and while a request runs, or a host lays its entry over, the
/// entry's deprecation raised before the value is taken; not at startup,
/// nor as the request ends and hands the entry back its value.
///
/// ```php
/// var_dump(ini_set('iconv.output_encoding', 'ASCII'), ini_set('iconv.output_encoding', str_repeat('a', 64)), ini_get('iconv.output_encoding'));
/// var_dump(iconv_set_encoding('internal_encoding', 'UTF-8'), ini_set('iconv.input_encoding', ''));
/// ```
/// ```expect
///
/// Deprecated: ini_set(): Use of iconv.output_encoding is deprecated in %s on line 2
/// string(0) ""
/// bool(false)
/// string(5) "ASCII"
///
/// Deprecated: iconv_set_encoding(): Use of iconv.internal_encoding is deprecated in %s on line 3
///
/// Deprecated: ini_set(): Use of iconv.input_encoding is deprecated in %s on line 3
/// bool(true)
/// string(0) ""
/// ```
fn on_modify(req: &mut Request, kind: Kind, value: &[u8]) -> bool {
    if value.len() >= MAX_NAME {
        return false;
    }
    if matches!(req.ini().stage(), IniStage::Activate | IniStage::Runtime) {
        let text = format!("Use of {} is deprecated", kind.entry());
        docref(req, Level::Deprecated, &text);
    }
    true
}

#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
fn on_input_encoding(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    Ok(on_modify(req, Kind::Input, value))
}

#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
fn on_output_encoding(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    Ok(on_modify(req, Kind::Output, value))
}

#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::on_modify`'s signature
fn on_internal_encoding(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    Ok(on_modify(req, Kind::Internal, value))
}

/// `get_input_encoding`, `get_output_encoding` and
/// `get_internal_encoding`: the `iconv.*` entry, then php's own, then
/// `default_charset`, then `UTF-8`.
fn encoding(req: &Request, kind: Kind) -> Vec<u8> {
    let own = kind.entry();
    [own.as_bytes(), kind.name().as_bytes(), b"default_charset"]
        .iter()
        .find_map(|name| {
            req.ini()
                .get(name)
                .filter(|v| !v.is_empty())
                .map(<[u8]>::to_vec)
        })
        .unwrap_or_else(|| b"UTF-8".to_vec())
}

/// The `?string $encoding` argument: the internal encoding for `null`,
/// and `None` after php's warning for a name of 64 bytes or more.
fn charset(req: &mut Request, given: Option<PhpString>) -> Option<Vec<u8>> {
    match given {
        None => Some(encoding(req, Kind::Internal)),
        Some(name) if name.len() >= MAX_NAME => {
            too_long(req);
            None
        }
        Some(name) => Some(name.as_bytes().to_vec()),
    }
}

fn too_long(req: &mut Request) {
    let text = "Encoding parameter exceeds the maximum allowed length of 64 characters";
    docref(req, Level::Warning, text);
}

/// `php_iconv_err_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Error {
    Converter,
    WrongCharset,
    IllegalChar,
    IllegalSeq,
    TooBig,
    Malformed,
    OutOfBounds,
    /// `PHP_ICONV_ERR_ALLOC`: a fallible ask the memory limit refused.
    Alloc,
    Unknown(Errno),
}

impl From<Errno> for Error {
    /// `errno` after `iconv`, as `php_iconv_string` reads it.
    fn from(errno: Errno) -> Error {
        match errno {
            Errno::Inval => Error::IllegalChar,
            Errno::Ilseq => Error::IllegalSeq,
            Errno::TooBig => Error::TooBig,
            Errno::Other(_) => Error::Unknown(errno),
        }
    }
}

/// `iconv_open`, its failure as php reads it.
fn opened(to: &[u8], from: &[u8]) -> Result<Cd, Error> {
    Cd::open(to, from).map_err(|errno| match errno {
        Errno::Inval => Error::WrongCharset,
        _ => Error::Converter,
    })
}

/// `errno` after a walk, as the character natives switch on it: room
/// is no error, and an `errno` the switch has no case for is `Unknown`
/// only where `strict`.
fn by_errno(errno: Option<Errno>, strict: bool) -> Result<(), Error> {
    match errno {
        None | Some(Errno::TooBig) => Ok(()),
        Some(Errno::Other(_)) if !strict => Ok(()),
        Some(errno) => Err(errno.into()),
    }
}

/// `_php_iconv_show_error`, and `false`.
#[rustfmt::skip]
fn failed<T>(req: &mut Request, error: Error, to: &[u8], from: &[u8]) -> Result<OrFalse<T>, Thrown> {
    let (from, to) = (String::from_utf8_lossy(from), String::from_utf8_lossy(to));
    let (notice, text) = match error {
        Error::OutOfBounds => return Err(arg_value_error(req, 3, "be contained in argument #1 ($haystack)")),
        Error::Converter => (false, "Cannot open converter".to_owned()),
        Error::WrongCharset => (false, format!("Wrong encoding, conversion from \"{from}\" to \"{to}\" is not allowed")),
        Error::IllegalChar => (true, "Detected an incomplete multibyte character in input string".to_owned()),
        Error::IllegalSeq => (true, "Detected an illegal character in input string".to_owned()),
        Error::TooBig => (false, "Buffer length exceeded".to_owned()),
        Error::Malformed => (false, "Malformed string".to_owned()),
        Error::Unknown(errno) => (true, format!("Unknown error ({})", errno.code())),
        Error::Alloc => (true, format!("Unknown error ({})", ffi::errno().code())),
    };
    docref(req, if notice { Level::Notice } else { Level::Warning }, &text);
    Ok(OrFalse::False)
}

/// `php_iconv_string`: the bytes, which every caller here drops on an
/// error.
fn convert(input: &[u8], to: &[u8], from: &[u8]) -> Result<Vec<u8>, Error> {
    let (out, result) = opened(to, from)?.convert(input);
    result.map(|()| out).map_err(Error::from)
}

/// `php_iconv_string` as php 8.5.12 makes it, into a string of the
/// request's heap: `in_len + 32` bytes asked for fallibly
/// (`zend_string_falloc`), grown by the input's length at each `E2BIG`
/// and by 16 while the shift state is flushed, each growth fallible
/// (`zend_string_fextend`); a refusal is php's `errno = 0` and so its
/// unknown error, the delayed OOM recorded. The string cut to what was
/// written, or the error, its string given back.
fn iconv_string(
    req: &mut Request,
    input: &[u8],
    to: &[u8],
    from: &[u8],
) -> Result<Result<FreshString, Error>, Thrown> {
    let mut cd = match opened(to, from) {
        Ok(cd) => cd,
        Err(error) => return Ok(Err(error)),
    };
    let refused = Err(Error::Unknown(Errno::Other(0)));
    let mut bsz = input.len() + 32;
    let Some(mut out) = req
        .memory_mut()
        .fallible(|req| FreshString::new(req, bsz))?
    else {
        return Ok(refused);
    };
    let (mut used, mut rest, mut result) = (0, input, Ok(0));
    while !rest.is_empty() {
        let (written, step) = cd.step(Some(&mut rest), &mut out.bytes_mut()[used..]);
        used += written;
        result = step;
        if result == Err(Errno::TooBig) && !rest.is_empty() {
            bsz += input.len();
            let Some(grown) = req.memory_mut().fallible(|req| out.extend(req, bsz))? else {
                return Ok(refused);
            };
            out = grown;
            continue;
        }
        break;
    }
    while result.is_ok() {
        let (written, step) = cd.step(None, &mut out.bytes_mut()[used..]);
        used += written;
        result = step;
        if result != Err(Errno::TooBig) {
            break;
        }
        bsz += 16;
        let Some(grown) = req.memory_mut().fallible(|req| out.extend(req, bsz))? else {
            return Ok(refused);
        };
        out = grown;
    }
    match result {
        Ok(_) => {
            out.set_len(used);
            Ok(Ok(out))
        }
        Err(errno) => {
            out.free(req);
            Ok(Err(Error::from(errno)))
        }
    }
}

/// `_php_iconv_strlen`: the characters counted eight bytes of
/// `UCS-4LE` room at a time, as php's `buf[GENERIC_SUPERSET_NBYTES*2]`
/// counts them, which a stateful encoding's escapes make a count of its
/// own.
fn strlen(input: &[u8], charset: &[u8]) -> Result<usize, Error> {
    let mut count = 0;
    let (errno, _) = opened(UCS4, charset)?.walk(input, 8, |bytes, _| {
        count += bytes.len() / 4;
        true
    });
    by_errno(errno, true).map(|()| count)
}

/// `iconv_strlen(string $string, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(iconv_strlen('héllo'), iconv_strlen("h\xe9llo", 'ISO-8859-1'), iconv_strlen("h\xffllo"), iconv_strlen("h\xc3"), iconv_strlen('x', 'nope'), iconv_strlen('x', str_repeat('a', 64)));
/// ```
/// ```expect
///
/// Notice: iconv_strlen(): Detected an illegal character in input string in %s on line 2
///
/// Notice: iconv_strlen(): Detected an incomplete multibyte character in input string in %s on line 2
///
/// Warning: iconv_strlen(): Wrong encoding, conversion from "nope" to "UCS-4LE" is not allowed in %s on line 2
///
/// Warning: iconv_strlen(): Encoding parameter exceeds the maximum allowed length of 64 characters in %s on line 2
/// int(5)
/// int(5)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn iconv_strlen(
    req: &mut Request,
    string: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    match strlen(string.as_bytes(), &charset) {
        Ok(n) => Ok(OrFalse::Value(i64::try_from(n).unwrap_or(0))),
        Err(error) => failed(req, error, UCS4, &charset),
    }
}

/// `_php_iconv_substr`: the offset and length clamped to the count,
/// then each character from the offset converted back through a second
/// converter opened at the first of them.
fn substr(input: &[u8], offset: i64, len: i64, charset: &[u8]) -> Result<Vec<u8>, Error> {
    let total = i64::try_from(strlen(input, charset)?).unwrap_or(i64::MAX);
    let offset = if offset < 0 {
        (offset + total).max(0)
    } else {
        offset.min(total)
    };
    let mut len = if len < 0 {
        (len + total - offset).max(0)
    } else {
        len.min(total)
    };
    len = len.min(total - offset);
    if len == 0 {
        return Ok(Vec::new());
    }
    let (mut back, mut out, mut refused, mut index) = (None::<Cd>, Vec::new(), None, 0);
    let (errno, _) = opened(UCS4, charset)?.walk(input, 4, |buf, errno| {
        index += 1;
        if index <= offset {
            return true;
        }
        if back.is_none() {
            match opened(charset, UCS4) {
                Ok(cd) => back = Some(cd),
                Err(error) => {
                    refused = Some(error);
                    return false;
                }
            }
        }
        match back.as_mut().map(|cd| cd.append(Some(buf), &mut out)) {
            Some(Ok(())) => {
                len -= 1;
                len > 0
            }
            Some(Err(e)) => {
                *errno = Some(e);
                false
            }
            None => false,
        }
    });
    if let Some(error) = refused {
        return Err(error);
    }
    by_errno(errno, false)?;
    if let Some(cd) = back.as_mut() {
        cd.append(None, &mut out).ok();
    }
    Ok(out)
}

/// `iconv_substr(string $string, int $offset, ?int $length = null, ?string $encoding = null): string|false`.
///
/// ```php
/// var_dump(iconv_substr('héllo wörld', 2, 5), iconv_substr('héllo', -3), iconv_substr('héllo', 1, -1), iconv_substr('héllo', 9), iconv_substr("h\xffllo", 1), bin2hex(iconv_substr("h\xe9llo", 1, 2, 'ISO-8859-1')));
/// ```
/// ```expect
///
/// Notice: iconv_substr(): Detected an illegal character in input string in %s on line 2
/// string(5) "llo w"
/// string(3) "llo"
/// string(4) "éll"
/// string(0) ""
/// bool(false)
/// string(4) "e96c"
/// ```
#[php_function]
fn iconv_substr(
    req: &mut Request,
    string: PhpString,
    offset: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    let input = string.as_bytes();
    let len = length.unwrap_or_else(|| i64::try_from(input.len()).unwrap_or(i64::MAX));
    match substr(input, offset, len, &charset) {
        Ok(bytes) => Ok(OrFalse::Value(PhpString::new(req, &bytes)?)),
        Err(error) => failed(req, error, UCS4, &charset),
    }
}

/// `_php_iconv_strpos`: the needle in `UCS-4LE`, matched character by
/// character from `offset` with php's fall back over the needle's own
/// prefix; `reverse` keeps the last match.
fn strpos(
    haystack: &[u8],
    needle: &[u8],
    offset: usize,
    charset: &[u8],
    reverse: bool,
) -> Result<Option<usize>, Error> {
    let ndl = convert(needle, UCS4, charset)?;
    let (mut at, mut start, mut found, mut index) = (0usize, None::<usize>, None, 0);
    let (_, errno) = opened(UCS4, charset)?.walk(haystack, 4, |buf, _| {
        index += 1;
        let this = index - 1;
        if this < offset {
            return true;
        }
        if buf == &ndl[at..at + 4] {
            start.get_or_insert(this);
            at += 4;
            if at == ndl.len() {
                found = start;
                (at, start) = (0, None);
                return reverse;
            }
            return true;
        }
        let (mut i, mut j, lim) = (0, 4, at);
        while j < lim {
            if ndl[j..j + 4] == ndl[i..i + 4] {
                i += 4;
            } else {
                j -= i;
                i = 0;
            }
            j += 4;
        }
        if buf == &ndl[i..i + 4] {
            start = start.map(|s| s + (lim - i) / 4);
            at = i + 4;
        } else {
            (at, start) = (0, None);
        }
        true
    });
    by_errno(errno, true)?;
    if offset > index {
        return Err(Error::OutOfBounds);
    }
    Ok(found)
}

/// The searches' answer: the position, or `false`.
fn position(
    req: &mut Request,
    found: Result<Option<usize>, Error>,
    charset: &[u8],
) -> Result<OrFalse<i64>, Thrown> {
    match found {
        Ok(Some(at)) => Ok(OrFalse::Value(i64::try_from(at).unwrap_or(0))),
        Ok(None) => Ok(OrFalse::False),
        Err(error) => failed(req, error, UCS4, charset),
    }
}

/// `iconv_strpos(string $haystack, string $needle, int $offset = 0, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(iconv_strpos('héllo wörld', 'ö'), iconv_strpos('aaab', 'aab'), iconv_strpos('héllo', 'l', -2), iconv_strpos('héllo', ''), iconv_strpos('héllo', 'z'));
/// try { iconv_strpos('abc', 'a', -9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { iconv_strpos('abc', 'a', 9); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(7)
/// int(1)
/// int(3)
/// bool(false)
/// bool(false)
/// iconv_strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// iconv_strpos(): Argument #3 ($offset) must be contained in argument #1 ($haystack)
/// ```
#[php_function]
fn iconv_strpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = 0)] mut offset: i64,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    if offset < 0 {
        match strlen(haystack, &charset) {
            Ok(n) => offset += i64::try_from(n).unwrap_or(0),
            Err(error) => return failed(req, error, UCS4, &charset),
        }
        if offset < 0 {
            let tail = "be contained in argument #1 ($haystack)";
            return Err(arg_value_error(req, 3, tail));
        }
    }
    if needle.is_empty() {
        return Ok(OrFalse::False);
    }
    let offset = usize::try_from(offset).unwrap_or(0);
    let found = strpos(haystack, needle, offset, &charset, false);
    position(req, found, &charset)
}

/// `iconv_strrpos(string $haystack, string $needle, ?string $encoding = null): int|false`.
///
/// ```php
/// var_dump(iconv_strrpos('aöaö', 'ö'), iconv_strrpos('abcabc', 'bc'), iconv_strrpos('abc', ''), iconv_strrpos('abc', 'z'));
/// ```
/// ```expect
/// int(3)
/// int(4)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn iconv_strrpos(
    req: &mut Request,
    haystack: PhpString,
    needle: PhpString,
    #[php(default = null)] encoding: Option<PhpString>,
) -> Result<OrFalse<i64>, Thrown> {
    if needle.is_empty() {
        return Ok(OrFalse::False);
    }
    let Some(charset) = charset(req, encoding) else {
        return Ok(OrFalse::False);
    };
    let found = strpos(haystack.as_bytes(), needle.as_bytes(), 0, &charset, true);
    position(req, found, &charset)
}

/// `iconv(string $from_encoding, string $to_encoding, string $string): string|false`,
/// `//TRANSLIT` and `//IGNORE` as the host's library reads them.
///
/// ```php
/// var_dump(bin2hex(iconv('UTF-8', 'ISO-8859-1', 'héllo')), bin2hex(iconv('UTF-8', 'UTF-16BE', 'ab')), iconv('UTF-8', 'ASCII//TRANSLIT', 'aébü€'), iconv('UTF-8', 'ASCII//IGNORE', "a\xffbé"));
/// var_dump(iconv('UTF-8', 'ASCII', 'aé'), iconv('UTF-8', 'nope', 'x'), iconv('UTF-8', 'ISO-8859-1', "a\xe2\x82"), iconv(str_repeat('a', 64), 'UTF-8', 'x'));
/// ```
/// ```expect
/// string(10) "68e96c6c6f"
/// string(8) "00610062"
/// string(9) "a'eb"uEUR"
/// string(2) "ab"
///
/// Notice: iconv(): Detected an illegal character in input string in %s on line 3
///
/// Warning: iconv(): Wrong encoding, conversion from "UTF-8" to "nope" is not allowed in %s on line 3
///
/// Notice: iconv(): Detected an incomplete multibyte character in input string in %s on line 3
///
/// Warning: iconv(): Encoding parameter exceeds the maximum allowed length of 64 characters in %s on line 3
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// Since 8.5.12 the string is asked for fallibly, and each growth, so a
/// conversion past the memory limit is refused there and the limit's fatal
/// is raised as the call returns:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(iconv('ASCII', 'UTF-32', str_repeat('a', 5 << 20)), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 5259264 bytes) in %s on line 3
/// ```
#[php_function]
fn iconv(
    req: &mut Request,
    from_encoding: PhpString,
    to_encoding: PhpString,
    string: PhpString,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let (from, to) = (from_encoding.as_bytes(), to_encoding.as_bytes());
    if from.len() >= MAX_NAME || to.len() >= MAX_NAME {
        too_long(req);
        return Ok(OrFalse::False);
    }
    match iconv_string(req, string.as_bytes(), to, from)? {
        Ok(out) => Ok(OrFalse::Value(out.finish())),
        Err(error) => failed(req, error, to, from),
    }
}

/// `php_iconv_output_handler` (`Z/ext/iconv/iconv.c:299`), what
/// `ob_start('ob_iconv_handler')` starts: at its start, refused once a
/// byte reached the host, else a `text/` mimetype, or the default one
/// while it is still to be sent, given `iconv.output_encoding`'s charset
/// in a `Content-Type` header, which makes the handler immutable; then
/// the buffer converted from the internal encoding to that one, what
/// converted going on beside php's notice for what did not.
///
/// ```php
/// ini_set('internal_encoding', 'ISO-8859-1');
/// ob_start('ob_iconv_handler'); echo "h\xe9llo\n"; ob_flush();
/// var_dump(ob_start('ob_iconv_handler'), ob_get_status()['flags'], ob_end_clean(), ob_list_handlers(), function_exists('ob_iconv_handler'));
/// echo "\xff\n";
/// ```
/// ```expect
/// héllo
///
/// Warning: ob_start(): Output handler 'ob_iconv_handler' cannot be used twice in %s on line 4
///
/// Notice: ob_start(): Failed to create buffer in %s on line 4
///
/// Notice: ob_end_clean(): Failed to discard buffer of ob_iconv_handler (0) in %s on line 4
/// bool(false)
/// int(20512)
/// bool(false)
/// array(1) {
///   [0]=>
///   string(16) "ob_iconv_handler"
/// }
/// bool(false)
/// ÿ
/// ```
///
/// After a byte reached the host the handler refuses at its start, is disabled,
/// and its buffer goes on as it was.
///
/// ```php
/// ini_set('output_encoding', 'ASCII');
/// echo "sent\n"; ob_start('ob_iconv_handler'); echo "é\n"; ob_flush(); var_dump(ob_get_status()['flags']);
/// ```
/// ```expect
/// sent
/// é
/// int(12400)
/// ```
fn ob_iconv_handler(
    req: &mut Request,
    call: &mut HandlerCall<'_>,
) -> Result<HandlerStatus, Thrown> {
    if call.op.start {
        if req.response().started_at().is_some() {
            return Ok(HandlerStatus::Failure);
        }
        let mimetype = match req.response().content_type() {
            ContentType::Held(held)
                if held.len() >= 5 && held[..5].eq_ignore_ascii_case(b"text/") =>
            {
                Some(
                    held.split(|&b| b == b';')
                        .next()
                        .unwrap_or_default()
                        .to_vec(),
                )
            }
            ContentType::Held(_) | ContentType::Unset => None,
            ContentType::Default => Some(
                req.ini()
                    .get(b"default_mimetype")
                    .map_or_else(|| b"text/html".to_vec(), <[u8]>::to_vec),
            ),
        };
        if let Some(mimetype) = mimetype
            && (!call.op.clean || !call.op.finish)
        {
            let charset = encoding(req, Kind::Output);
            let cut = charset
                .windows(2)
                .position(|w| w == b"//")
                .unwrap_or(charset.len());
            let mut line = b"Content-Type:".to_vec();
            line.extend_from_slice(&mimetype);
            line.extend_from_slice(b"; charset=");
            line.extend_from_slice(&charset[..cut]);
            call.immutable = req.response_mut().header_add(&line, true, 0);
        }
    }
    if !call.input.is_empty() {
        let (to, from) = (encoding(req, Kind::Output), encoding(req, Kind::Internal));
        let (out, error) = match opened(&to, &from) {
            Ok(mut cd) => match cd.convert(call.input) {
                (out, Ok(())) => (out, None),
                (_, Err(errno @ Errno::Other(_))) => (Vec::new(), Some(Error::from(errno))),
                (out, Err(errno)) => (out, Some(Error::from(errno))),
            },
            Err(error) => (Vec::new(), Some(error)),
        };
        if let Some(error) = error {
            let _ = failed::<()>(req, error, &to, &from)?;
        }
        call.out = out;
    }
    Ok(HandlerStatus::Success)
}

/// `iconv_set_encoding(string $type, string $encoding): bool`, which
/// sets the `iconv.*` entry.
///
/// ```php
/// var_dump(@iconv_set_encoding('internal_encoding', 'ISO-8859-1'), iconv_get_encoding('internal_encoding'), iconv_strlen("h\xe9"), iconv_set_encoding('nope', 'x'), iconv_set_encoding('input_encoding', str_repeat('a', 64)));
/// ```
/// ```expect
///
/// Warning: iconv_set_encoding(): Encoding parameter exceeds the maximum allowed length of 64 characters in %s on line 2
/// bool(true)
/// string(10) "ISO-8859-1"
/// int(2)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn iconv_set_encoding(
    req: &mut Request,
    r#type: PhpString,
    encoding: PhpString,
) -> Result<bool, Thrown> {
    if encoding.len() >= MAX_NAME {
        too_long(req);
        return Ok(false);
    }
    let Some(kind) = Kind::parse(r#type.as_bytes()) else {
        return Ok(false);
    };
    // The entry's rule ([`on_modify`]) raises its deprecation.
    Ok(req
        .ini_mut()
        .set_string(kind.entry().as_bytes(), encoding)?
        .is_some())
}

/// `iconv_get_encoding(string $type = "all"): array|string|false`.
///
/// ```php
/// var_dump(iconv_get_encoding(), iconv_get_encoding('OUTPUT_encoding'), iconv_get_encoding('nope'));
/// ini_set('default_charset', 'ISO-8859-1'); var_dump(iconv_get_encoding('input_encoding'));
/// ```
/// ```expect
/// array(3) {
///   ["input_encoding"]=>
///   string(5) "UTF-8"
///   ["output_encoding"]=>
///   string(5) "UTF-8"
///   ["internal_encoding"]=>
///   string(5) "UTF-8"
/// }
/// string(5) "UTF-8"
/// bool(false)
/// string(10) "ISO-8859-1"
/// ```
#[php_function]
// The declared `array|string|false`, each fresh value an owner.
#[allow(clippy::type_complexity)]
fn iconv_get_encoding(
    req: &mut Request,
    #[php(default = "all")] r#type: PhpString,
) -> Result<OrFalse<Union<Owner<PhpArray>, Owner<PhpString>>>, Thrown> {
    let kind = r#type.as_bytes();
    if kind.eq_ignore_ascii_case(b"all") {
        let mut out = PhpArray::new(req)?;
        for kind in Kind::ALL {
            let charset = encoding(req, kind);
            let value = PhpString::new(req, &charset)?;
            out.set_str(req, kind.name().as_bytes(), value.into_value())?;
        }
        return Ok(OrFalse::Value(Union::A(out)));
    }
    match Kind::parse(kind) {
        Some(kind) => {
            let charset = encoding(req, kind);
            Ok(OrFalse::Value(Union::B(PhpString::new(req, &charset)?)))
        }
        None => Ok(OrFalse::False),
    }
}

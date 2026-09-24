//! The `iconv` extension: `php-src/ext/iconv` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, `libc`, for the host's
//! `iconv(3)` in `ffi`; one entry, `extension()`; its state on the value
//! (ADR-0005). The phpt subset is `php-src/ext/iconv/tests`, through
//! `xtask phpt --subset`.
//!
//! `iconv.c`'s conversions, character counts, substrings and searches
//! over `UCS-4LE`, php's `GENERIC_SUPERSET_NAME`, one converter per
//! call; the three `iconv.*` ini entries, which fall back on php's own
//! `input_encoding`, `internal_encoding` and `output_encoding` and then on
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

use ffi::{Cd, Errno};
use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::coerce::coerce_param;
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, IniDecl, Name, Parameter, Type};
use php_ext::raising::{Level, Thrown, arg_count_error, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// Engine-scoped and empty: building it is `zend_reset_lc_ctype_locale`,
/// which `php_module_startup` runs.
struct Startup;

type Entry = fn(&mut Request) -> Result<Value, Thrown>;

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

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
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
pub fn extension() -> Extension {
    let constant = |name: &str, value| ConstDecl {
        name: Name(name.as_bytes().into()),
        value,
        attributes: Vec::new(),
    };
    let ini = |kind: Kind| IniDecl {
        name: Name(kind.entry().into_bytes().into()),
        default: Box::new([]),
        user: true,
        per_dir: true,
        system: true,
    };
    ExtensionBuilder::default()
        .functions(natives())
        .constants(vec![
            constant("ICONV_IMPL", ConstExpr::Str(b"libiconv".as_slice().into())),
            constant("ICONV_VERSION", ConstExpr::Str(b"1.11".as_slice().into())),
            constant("ICONV_MIME_DECODE_STRICT", ConstExpr::Long(1)),
            constant("ICONV_MIME_DECODE_CONTINUE_ON_ERROR", ConstExpr::Long(2)),
        ])
        .ini(Kind::ALL.map(ini).to_vec())
        .info(info)
        .engine_state(|_| {
            ffi::reset_lc_ctype();
            Startup
        })
        .request_state::<State>()
        .build("iconv")
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
    req.info_table(&[
        &["iconv support", "enabled"],
        &["iconv implementation", "libiconv"],
        &["iconv library version", "1.11"],
    ]);
    let names = Kind::ALL.map(Kind::entry);
    req.info_ini_entries(&names.each_ref().map(String::as_bytes));
}

/// One declared type: `?T`, or `A|B`.
fn ty(text: &str) -> Type {
    if let Some(inner) = text.strip_prefix('?') {
        return Type::Nullable(Box::new(ty(inner)));
    }
    let mut members: Vec<Type> = text
        .split('|')
        .map(|word| match word {
            "string" => Type::String,
            "int" => Type::Int,
            "bool" => Type::Bool,
            "false" => Type::False,
            _ => Type::Array,
        })
        .collect();
    if members.len() == 1 {
        members.remove(0)
    } else {
        Type::Union(members)
    }
}

/// A declaration off `iconv.stub.php`, its parameters `(name, type,
/// default)`.
fn declared(
    name: &str,
    parameters: &[(&str, &str, Option<ConstExpr>)],
    returns: &str,
) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.as_bytes().into()),
        parameters: parameters
            .iter()
            .map(|(name, t, default)| Parameter {
                name: Name(name.as_bytes().into()),
                ty: Some(ty(t)),
                by_ref: false,
                prefer_ref: false,
                variadic: false,
                default: default.clone(),
                attributes: Vec::new(),
            })
            .collect(),
        return_type: Some(ty(returns)),
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// The natives in the stub's order.
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
#[rustfmt::skip]
fn natives() -> Vec<(FunctionDecl, Entry)> {
    let null = || Some(ConstExpr::Null);
    let enc = || ("encoding", "?string", Some(ConstExpr::Null));
    vec![
        (declared("iconv_strlen", &[("string", "string", None), enc()], "int|false"), iconv_strlen as Entry),
        (declared("iconv_substr", &[("string", "string", None), ("offset", "int", None), ("length", "?int", null()), enc()], "string|false"), iconv_substr),
        (declared("iconv_strpos", &[("haystack", "string", None), ("needle", "string", None), ("offset", "int", Some(ConstExpr::Long(0))), enc()], "int|false"), iconv_strpos),
        (declared("iconv_strrpos", &[("haystack", "string", None), ("needle", "string", None), enc()], "int|false"), iconv_strrpos),
        (declared("iconv", &[("from_encoding", "string", None), ("to_encoding", "string", None), ("string", "string", None)], "string|false"), iconv),
        (declared("iconv_set_encoding", &[("type", "string", None), ("encoding", "string", None)], "bool"), iconv_set_encoding),
        (declared("iconv_get_encoding", &[("type", "string", Some(ConstExpr::Str(b"all".as_slice().into())))], "array|string|false"), iconv_get_encoding),
    ]
}

/// `ZEND_PARSE_PARAMETERS`' count check.
fn count(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (word, bound) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let plural = if bound == 1 { "" } else { "s" };
    let tail = format!("{word} {bound} argument{plural}, {given} given");
    Err(arg_count_error(req, &tail))
}

/// The argument at `position` coerced to `ty`; `None` past the count
/// and for a `null` the type took.
fn argument(req: &mut Request, position: u32, ty: &Type) -> Result<Option<Value>, Thrown> {
    match req.argument(position) {
        Some(value) if !matches!(value.as_ref(), ValueRef::Undef) => {
            let value = coerce_param(req, &value, ty, position)?;
            Ok((!matches!(value.as_ref(), ValueRef::Null)).then_some(value))
        }
        _ => Ok(None),
    }
}

/// `ty`, or with `nullable` its `?ty`.
#[rustfmt::skip]
fn or_null(ty: Type, nullable: bool) -> Type {
    if nullable { Type::Nullable(Box::new(ty)) } else { ty }
}

/// `Z_PARAM_STR`, and with `nullable` its `_OR_NULL`.
fn string(req: &mut Request, position: u32, nullable: bool) -> Result<Option<Vec<u8>>, Thrown> {
    let value = argument(req, position, &or_null(Type::String, nullable))?;
    let bytes = value.and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec));
    Ok(bytes.or((!nullable).then(Vec::new)))
}

/// `Z_PARAM_LONG`, and with `nullable` its `_OR_NULL`.
fn long(req: &mut Request, position: u32, nullable: bool) -> Result<Option<i64>, Thrown> {
    Ok(argument(req, position, &or_null(Type::Int, nullable))?.and_then(|v| v.as_int()))
}

/// `get_input_encoding`, `get_output_encoding` and
/// `get_internal_encoding`: the `iconv.*` entry, then php's own, then
/// `default_charset`, then `UTF-8`.
fn encoding(req: &Request, kind: Kind) -> Vec<u8> {
    let own = kind.entry();
    [own.as_bytes(), kind.name().as_bytes(), b"default_charset"]
        .iter()
        .find_map(|name| {
            req.ini_get(name)
                .filter(|v| !v.is_empty())
                .map(<[u8]>::to_vec)
        })
        .unwrap_or_else(|| b"UTF-8".to_vec())
}

/// The `?string $encoding` slot: the internal encoding for `null`, and
/// `None` after php's warning for a name of 64 bytes or more.
fn charset(req: &mut Request, position: u32) -> Result<Option<Vec<u8>>, Thrown> {
    match string(req, position, true)? {
        None => Ok(Some(encoding(req, Kind::Internal))),
        Some(name) if name.len() >= MAX_NAME => {
            too_long(req);
            Ok(None)
        }
        Some(name) => Ok(Some(name)),
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
    OutOfBounds,
    Unknown(i32),
}

impl From<Errno> for Error {
    /// `errno` after `iconv`, as `php_iconv_string` reads it.
    fn from(errno: Errno) -> Error {
        match errno {
            Errno::Inval => Error::IllegalChar,
            Errno::Ilseq => Error::IllegalSeq,
            Errno::TooBig => Error::TooBig,
            Errno::Other(n) => Error::Unknown(n),
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
fn failed(req: &mut Request, error: Error, to: &[u8], from: &[u8]) -> Result<Value, Thrown> {
    let (from, to) = (String::from_utf8_lossy(from), String::from_utf8_lossy(to));
    let (notice, text) = match error {
        Error::OutOfBounds => return Err(arg_value_error(req, 3, "be contained in argument #1 ($haystack)")),
        Error::Converter => (false, "Cannot open converter".to_owned()),
        Error::WrongCharset => (false, format!("Wrong encoding, conversion from \"{from}\" to \"{to}\" is not allowed")),
        Error::IllegalChar => (true, "Detected an incomplete multibyte character in input string".to_owned()),
        Error::IllegalSeq => (true, "Detected an illegal character in input string".to_owned()),
        Error::TooBig => (false, "Buffer length exceeded".to_owned()),
        Error::Unknown(errno) => (true, format!("Unknown error ({errno})")),
    };
    docref(req, if notice { Level::Notice } else { Level::Warning }, &text);
    Ok(Value::bool(false))
}

/// `php_iconv_string`: the bytes, which every caller here drops on an
/// error.
fn convert(input: &[u8], to: &[u8], from: &[u8]) -> Result<Vec<u8>, Error> {
    let (out, result) = opened(to, from)?.convert(input);
    result.map(|()| out).map_err(Error::from)
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
fn iconv_strlen(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 1, 2)?;
    let input = string(req, 1, false)?.unwrap_or_default();
    let Some(charset) = charset(req, 2)? else {
        return Ok(Value::bool(false));
    };
    match strlen(&input, &charset) {
        Ok(n) => Ok(Value::int(i64::try_from(n).unwrap_or(0))),
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
        let _ = cd.append(None, &mut out);
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
fn iconv_substr(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 4)?;
    let input = string(req, 1, false)?.unwrap_or_default();
    let offset = long(req, 2, false)?.unwrap_or(0);
    let len = long(req, 3, true)?;
    let Some(charset) = charset(req, 4)? else {
        return Ok(Value::bool(false));
    };
    let len = len.unwrap_or_else(|| i64::try_from(input.len()).unwrap_or(i64::MAX));
    match substr(&input, offset, len, &charset) {
        Ok(bytes) => Ok(Value::string(PhpString::new(req, &bytes)?)),
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
) -> Result<Value, Thrown> {
    match found {
        Ok(Some(at)) => Ok(Value::int(i64::try_from(at).unwrap_or(0))),
        Ok(None) => Ok(Value::bool(false)),
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
fn iconv_strpos(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 4)?;
    let haystack = string(req, 1, false)?.unwrap_or_default();
    let needle = string(req, 2, false)?.unwrap_or_default();
    let mut offset = long(req, 3, false)?.unwrap_or(0);
    let Some(charset) = charset(req, 4)? else {
        return Ok(Value::bool(false));
    };
    if offset < 0 {
        match strlen(&haystack, &charset) {
            Ok(n) => offset += i64::try_from(n).unwrap_or(0),
            Err(error) => return failed(req, error, UCS4, &charset),
        }
        if offset < 0 {
            let tail = "be contained in argument #1 ($haystack)";
            return Err(arg_value_error(req, 3, tail));
        }
    }
    if needle.is_empty() {
        return Ok(Value::bool(false));
    }
    let offset = usize::try_from(offset).unwrap_or(0);
    let found = strpos(&haystack, &needle, offset, &charset, false);
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
fn iconv_strrpos(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 3)?;
    let haystack = string(req, 1, false)?.unwrap_or_default();
    let needle = string(req, 2, false)?.unwrap_or_default();
    if needle.is_empty() {
        return Ok(Value::bool(false));
    }
    let Some(charset) = charset(req, 3)? else {
        return Ok(Value::bool(false));
    };
    let found = strpos(&haystack, &needle, 0, &charset, true);
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
fn iconv(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 3, 3)?;
    let from = string(req, 1, false)?.unwrap_or_default();
    let to = string(req, 2, false)?.unwrap_or_default();
    let input = string(req, 3, false)?.unwrap_or_default();
    if from.len() >= MAX_NAME || to.len() >= MAX_NAME {
        too_long(req);
        return Ok(Value::bool(false));
    }
    match convert(&input, &to, &from) {
        Ok(out) => Ok(Value::string(PhpString::new(req, &out)?)),
        Err(error) => failed(req, error, &to, &from),
    }
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
fn iconv_set_encoding(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 2, 2)?;
    let kind = string(req, 1, false)?.unwrap_or_default();
    let charset = string(req, 2, false)?.unwrap_or_default();
    if charset.len() >= MAX_NAME {
        too_long(req);
        return Ok(Value::bool(false));
    }
    let Some(kind) = Kind::parse(&kind) else {
        return Ok(Value::bool(false));
    };
    let name = kind.entry();
    // `OnUpdateInternalEncoding` and its kin, at the runtime stage.
    docref(
        req,
        Level::Deprecated,
        &format!("Use of {name} is deprecated"),
    );
    Ok(Value::bool(
        req.ini_set(name.as_bytes(), &charset)?.is_some(),
    ))
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
fn iconv_get_encoding(req: &mut Request) -> Result<Value, Thrown> {
    count(req, 0, 1)?;
    let kind = string(req, 1, true)?.unwrap_or_else(|| b"all".to_vec());
    if kind.eq_ignore_ascii_case(b"all") {
        let mut out = PhpArray::new(req)?;
        for kind in Kind::ALL {
            let key = PhpString::new(req, kind.name().as_bytes())?;
            let charset = encoding(req, kind);
            let value = PhpString::new(req, &charset)?;
            out.set(req, Key::Str(key), Value::string(value))?;
        }
        return Ok(Value::array(out));
    }
    match Kind::parse(&kind) {
        Some(kind) => {
            let charset = encoding(req, kind);
            Ok(Value::string(PhpString::new(req, &charset)?))
        }
        None => Ok(Value::bool(false)),
    }
}

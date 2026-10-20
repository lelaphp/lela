//! `mb_gpc.c` and the HTTP natives of `mbstring.c`: `mb_parse_str`,
//! which parses a query string in the request's input encoding and
//! converts each name and value to the internal one; `mb_http_input`;
//! and `mb_output_handler`, which converts a buffer to the HTTP response
//! encoding where the response's content type is one
//! `mbstring.http_output_conv_mimetypes` names.

use crate::convert::guess;
use crate::ffi::mbfl::{Encoder, Encoding};
use crate::settings::{Source, State, settings};
use crate::text::new_string;
use crate::text::value_error;
use php_ext::abi::{OrFalse, Ref, Union, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{ContentType, Ini, Input, Request};
use php_ext::values::{Owner, PhpArray, PhpString, Value};
use php_ext_pcre::Regex;

/// `PHP_OUTPUT_HANDLER_START` and `PHP_OUTPUT_HANDLER_FINAL`.
const HANDLER_START: i64 = 0x01;
const HANDLER_FINAL: i64 = 0x08;

/// `_php_mb_compile_regex` of `mbstring.http_output_conv_mimetypes`,
/// trimmed as `php_trim` trims it: `pcre2_compile` with
/// `PCRE2_CASELESS`, which ext/pcre's syntax writes as a pattern between
/// delimiters with the `i` modifier. The empty pattern is no pattern;
/// one that does not compile is refused, after pcre's warning.
pub(crate) fn conv_mimetypes(req: &mut Request, value: &[u8]) -> Result<Option<Regex>, Refused> {
    let blank = |b: &u8| b" \t\n\r\x0b\0".contains(b);
    let start = value.iter().position(|b| !blank(b)).unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|b| !blank(b))
        .map_or(start, |e| e + 1);
    let pattern = &value[start..end];
    if pattern.is_empty() {
        return Ok(None);
    }
    let delimiter = [0x01u8, b'~', b'#', b'%', b'!', b'@', b'/']
        .into_iter()
        .find(|d| !pattern.contains(d))
        .ok_or(Refused)?;
    let mut written = vec![delimiter];
    written.extend_from_slice(pattern);
    written.extend_from_slice(&[delimiter, b'i']);
    Regex::compile(req, &written).map(Some).ok_or(Refused)
}

/// A pattern `mbstring.http_output_conv_mimetypes` cannot take.
pub(crate) struct Refused;

/// A hex digit's value.
fn hex(c: u8) -> Option<u8> {
    char::from(c)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// `php_url_decode`: `+` a space, `%XX` its byte.
fn url_decode(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'+' => out.push(b' '),
            b'%' if text.len() - i > 2 => match (hex(text[i + 1]), hex(text[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h << 4) | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// The variables a source reads to, each name and value as it is to be
/// registered.
pub(crate) type Variables = Vec<(Vec<u8>, Vec<u8>)>;

/// `_php_mb_encoding_handler_ex`: the variables of `text`, split on any
/// of `separators` and url-decoded, their encoding detected over every
/// name and value together where more than one is listed, and each
/// converted to `to`; with the encoding it read them in, none where it
/// read none.
pub(crate) fn encoding_handler(
    req: &mut Request,
    text: &[u8],
    separators: &[u8],
    to: Encoding,
    from_list: &[Encoding],
    report_errors: bool,
) -> (Variables, Option<Encoding>) {
    let text = php_ext::coerce::c_str(text);
    if text.is_empty() {
        return (Vec::new(), None);
    }
    let mut pairs: Variables = Vec::new();
    for var in text
        .split(|b| separators.contains(b))
        .filter(|v| !v.is_empty())
    {
        match var.iter().position(|&b| b == b'=') {
            Some(eq) => pairs.push((url_decode(&var[..eq]), url_decode(&var[eq + 1..]))),
            None => pairs.push((url_decode(var), Vec::new())),
        }
    }
    // `PG(max_input_vars)`, what `OnUpdateLongGEZero` stored: the
    // quantity rule's reading of the entry.
    let max_input_vars = req.ini().get(b"max_input_vars").map_or(0, Ini::parse_bytes);
    if i64::try_from(pairs.len()).unwrap_or(i64::MAX) > max_input_vars {
        let text = format!(
            "Input variables exceeded {max_input_vars}. To increase the limit change max_input_vars in php.ini."
        );
        docref(req, Level::Warning, &text);
        return (Vec::new(), None);
    }
    let from = match from_list {
        [] => Encoding::pass(),
        [one] => *one,
        _ => {
            let strict = settings(req).strict_detection;
            let strings: Vec<&[u8]> = pairs
                .iter()
                .flat_map(|(n, v)| [n.as_slice(), v.as_slice()])
                .collect();
            guess(&strings, from_list, strict, false).unwrap_or_else(|| {
                if report_errors {
                    docref(req, Level::Warning, "Unable to detect encoding");
                }
                Encoding::pass()
            })
        }
    };
    if from != Encoding::pass() && to != Encoding::pass() {
        for (name, value) in &mut pairs {
            *name = crate::convert::convert_counted(req, name, from, to);
            *value = crate::convert::convert_counted(req, value, from, to);
        }
    }
    (pairs, Some(from))
}

/// `mbstr_treat_data`, with `php_mb_post_handler` for a form body: under
/// `mbstring.encoding_translation`, a query string's, a cookie header's,
/// a form body's and `parse_str()`'s variables read in the HTTP input
/// encodings and converted to the ini's internal encoding, what they
/// were read in kept for `mb_http_input()`; none otherwise, which leaves
/// the source to `php_default_treat_data`.
#[allow(clippy::unnecessary_wraps)] // `ExtensionBuilder::treat_data`'s signature
pub(crate) fn treat_data(
    req: &mut Request,
    input: Input,
    source: &[u8],
) -> Result<Option<Variables>, Thrown> {
    let (translate, to, from_list) = {
        let s = settings(req);
        (s.encoding_translation, s.internal, s.http_input.clone())
    };
    let (slot, separators) = match input {
        Input::Get => (
            Source::Get,
            req.ini()
                .get(b"arg_separator.input")
                .unwrap_or(b"&")
                .to_vec(),
        ),
        Input::String => (
            Source::String,
            req.ini()
                .get(b"arg_separator.input")
                .unwrap_or(b"&")
                .to_vec(),
        ),
        Input::Cookie => (Source::Cookie, b";".to_vec()),
        Input::Post => (Source::Post, b"&".to_vec()),
        Input::Server | Input::Env => return Ok(None),
    };
    if !translate {
        return Ok(None);
    }
    let state = req.state::<State>();
    *state.identified_as(slot) = None;
    if input != Input::Post {
        state.illegal_chars = 0;
    }
    let (variables, detected) = encoding_handler(req, source, &separators, to, &from_list, false);
    let state = req.state::<State>();
    state.identified = detected;
    if detected.is_some() {
        *state.identified_as(slot) = detected;
    }
    Ok(Some(variables))
}

/// `mb_parse_str(string $string, &$result): bool`.
///
/// ```php
/// mb_parse_str("a=%C3%B1&b[]=x+y&c", $r); var_dump($r);
/// ```
/// ```expect
/// array(3) {
///   ["a"]=>
///   string(2) "ñ"
///   ["b"]=>
///   array(1) {
///     [0]=>
///     string(3) "x y"
///   }
///   ["c"]=>
///   string(0) ""
/// }
/// ```
///
/// `max_input_vars` is `PG(max_input_vars)`, the quantity rule's
/// reading, so `0x1` is one:
///
/// ```php
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'max_input_vars=0x1', '-r', 'var_dump(mb_parse_str("a=1&b=2", $r), $r);'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
/// ```
/// ```expect process
/// Warning: mb_parse_str(): Input variables exceeded 1. To increase the limit change max_input_vars in php.ini. in Command line code on line 1
/// bool(false)
/// array(0) {
/// }
/// 0
/// ```
#[php_function]
fn mb_parse_str(
    req: &mut Request,
    string: PhpString,
    result: Ref<Untyped<Value>>,
) -> Result<bool, Thrown> {
    let made = PhpArray::new(req)?;
    let array = *made;
    result.set(req, Untyped(made.into_value()))?;
    let into = match result.reference() {
        Some(cell) => cell.separate_array(req)?.unwrap_or(array),
        None => array,
    };
    let separators = req
        .ini()
        .get(b"arg_separator.input")
        .unwrap_or(b"&")
        .to_vec();
    let (to, from_list) = {
        let s = settings(req);
        (s.current_internal, s.http_input.clone())
    };
    let (variables, detected) =
        encoding_handler(req, string.as_bytes(), &separators, to, &from_list, true);
    for (name, value) in variables {
        // `php_register_variable_safe`'s `ZVAL_STRINGL_FAST`.
        let value = crate::text::cut_string(req, &value)?;
        req.variables_mut()
            .register_variable(into, &name, value.into_value())?;
    }
    req.state::<State>().identified = detected;
    Ok(detected.is_some())
}

/// `mb_http_input(?string $type = null): array|string|false`.
///
/// ```php
/// var_dump(mb_http_input(), mb_http_input("I"), mb_http_input("L"), mb_http_input("G"));
/// try { mb_http_input("X"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// array(1) {
///   [0]=>
///   string(5) "UTF-8"
/// }
/// string(5) "UTF-8"
/// bool(false)
/// mb_http_input(): Argument #1 ($type) must be one of "G", "P", "C", "S", "I", or "L"
/// ```
#[php_function]
// The declared `array|string|false`, each fresh value an owner.
#[allow(clippy::type_complexity)]
fn mb_http_input(
    req: &mut Request,
    #[php(default = null)] r#type: Option<PhpString>,
) -> Result<OrFalse<Union<Owner<PhpArray>, Owner<PhpString>>>, Thrown> {
    let refused = "must be one of \"G\", \"P\", \"C\", \"S\", \"I\", or \"L\"";
    let encoding = match r#type.as_ref().map(PhpString::as_bytes) {
        None => req.state::<State>().identified,
        Some([t]) => match t.to_ascii_uppercase() {
            b'G' => *req.state::<State>().identified_as(Source::Get),
            b'P' => *req.state::<State>().identified_as(Source::Post),
            b'C' => *req.state::<State>().identified_as(Source::Cookie),
            b'S' => *req.state::<State>().identified_as(Source::String),
            b'I' => {
                let list = settings(req).http_input.clone();
                let mut out = PhpArray::new(req)?;
                for enc in list {
                    let name = new_string(req, enc.name())?;
                    out.append(req, name.into_value())?;
                }
                return Ok(OrFalse::Value(Union::A(out)));
            }
            b'L' => {
                let list = settings(req).http_input.clone();
                if list.is_empty() {
                    return Ok(OrFalse::False);
                }
                let names: Vec<&[u8]> = list.iter().map(|e| e.name()).collect();
                return Ok(OrFalse::Value(Union::B(new_string(
                    req,
                    &names.join(&b","[..]),
                )?)));
            }
            _ => return Err(value_error(req, 1, refused)),
        },
        Some(_) => return Err(value_error(req, 1, refused)),
    };
    match encoding {
        Some(enc) => Ok(OrFalse::Value(Union::B(new_string(req, enc.name())?))),
        None => Ok(OrFalse::False),
    }
}

/// `mb_output_handler(string $string, int $status): string`: the buffer
/// converted from the internal encoding to the HTTP response encoding,
/// once its first call found a content type to convert. php adds the
/// `Content-Type` header with the charset only when called outside an
/// buffer's handler; served as one, as `ob_start()` serves it, it adds none.
///
/// ```php
/// ob_start("mb_output_handler"); mb_http_output("ISO-8859-1"); echo "a€\n"; ob_end_flush();
/// var_dump(mb_output_handler("x", 0));
/// ```
/// ```expect
/// a?
/// string(1) "x"
/// ```
///
/// Called outside any handler, the `START` arm adds the `Content-Type`
/// with its charset through `sapi_add_header`, whose own refusal is
/// php's warning once the body has started.
///
/// ```php
/// echo "a\n"; var_dump(mb_output_handler('x', PHP_OUTPUT_HANDLER_START));
/// ```
/// ```expect
/// a
///
/// Warning: Cannot modify header information - headers already sent by (output started at %s:2) in %s on line 2
/// string(1) "x"
/// ```
///
/// The decoder keeps what the `START` chunk set, its byte-order mark, for
/// the `FINAL` one, and each chunk is encoded afresh under the settings
/// standing when it comes, the substitute among them: a stateful
/// target's shift is opened again in the `FINAL` chunk and closed there,
/// and the shift a `START` chunk left open stays open where the `FINAL`
/// chunk is empty.
///
/// ```php
/// $rows = [];
/// mb_internal_encoding('UTF-16');
/// mb_http_output('JIS');
/// $rows[] = mb_output_handler("\xfe\xff\x30\x42", PHP_OUTPUT_HANDLER_START);
/// $rows[] = mb_output_handler("\x30\x44", PHP_OUTPUT_HANDLER_FINAL);
/// mb_internal_encoding('UTF-16');
/// mb_output_handler("\xfe\xff\x30\x42", PHP_OUTPUT_HANDLER_START);
/// mb_http_output('ASCII');
/// mb_internal_encoding('UTF-16BE');
/// mb_substitute_character(0x21);
/// $rows[] = mb_output_handler("\x30\x44", PHP_OUTPUT_HANDLER_FINAL);
/// mb_internal_encoding('UTF-8');
/// mb_http_output('JIS');
/// $rows[] = mb_output_handler('あ', PHP_OUTPUT_HANDLER_START) . mb_output_handler('', PHP_OUTPUT_HANDLER_FINAL);
/// foreach ($rows as $row) echo bin2hex($row), "\n";
/// ```
/// ```expect
/// 1b24422422
/// 1b244224241b2842
/// 21
/// 1b24422422
/// ```
#[php_function]
fn mb_output_handler(
    req: &mut Request,
    string: PhpString,
    status: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let (encoding, internal, sub) = {
        let s = settings(req);
        (s.http_output, s.current_internal, s.substitute)
    };
    if encoding == Encoding::pass() {
        return Ok(crate::text::same(req, string));
    }
    if status & HANDLER_START != 0
        && let Some(mimetype) = start_mimetype(req)
    {
        // `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_GET_FLAGS)`
        // answers while a handler is being served, which adds no header.
        if let Some(charset) = encoding.preferred_mime_name()
            && req.response().running_handler_flags().is_none()
        {
            let mut line = b"Content-Type: ".to_vec();
            line.extend_from_slice(&mimetype);
            line.extend_from_slice(b"; charset=");
            line.extend_from_slice(charset);
            req.response_mut().header_add(&line, true, 0);
        }
        req.state::<State>().outconv = Some(0);
    }
    let Some(mut state) = req.state::<State>().outconv else {
        return Ok(crate::text::same(req, string));
    };
    let last = status & HANDLER_FINAL != 0;
    let mut out = Encoder::new(encoding, string.len(), sub.char, sub.raw_mode());
    let mut rest = string.as_bytes();
    let mut buf = [0u32; 128];
    while !rest.is_empty() {
        let n = internal.decode(&mut rest, &mut buf, &mut state);
        out.write(&buf[..n], rest.is_empty() && last);
    }
    let errors = out.errors();
    let bytes = out.into_vec();
    let st = req.state::<State>();
    st.illegal_chars += u64::from(errors);
    st.outconv = if last { None } else { Some(state) };
    new_string(req, &bytes)
}

/// The mimetype `mb_output_handler`'s `START` arm converts under, and
/// `None` where it converts nothing: a held one `mbstring.http_output_conv_mimetypes`
/// matches, cut at its `;`, or `default_mimetype` while the default is
/// still to be sent (`SG(default_mimetype)`, `text/html` where unset).
fn start_mimetype(req: &mut Request) -> Option<Vec<u8>> {
    match req.response().content_type() {
        ContentType::Held(held) => {
            let pattern = req
                .ini()
                .get(b"mbstring.http_output_conv_mimetypes")
                .unwrap_or_default()
                .to_vec();
            match conv_mimetypes(req, &pattern) {
                Ok(Some(regex)) if regex.is_match(req, &held) => {
                    let end = held.iter().position(|b| *b == b';').unwrap_or(held.len());
                    Some(held[..end].to_vec())
                }
                _ => None,
            }
        }
        ContentType::Default => Some(
            req.ini()
                .get(b"default_mimetype")
                .map_or_else(|| b"text/html".to_vec(), <[u8]>::to_vec),
        ),
        ContentType::Unset => None,
    }
}

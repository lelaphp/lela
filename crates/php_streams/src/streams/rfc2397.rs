//! The `data:` wrapper, php's `php_stream_rfc2397_wrapper`
//! (`Z/main/streams/memory.c:612`): RFC 2397's `data:[<mediatype>]
//! [;base64],<data>`, with `data://` taken as `data:`, opened as a temp
//! stream holding the data, under the label `RFC2397`, whose
//! `stream_get_meta_data` starts with what the URL said: the media type,
//! each parameter, and whether the data was base64.

use super::{MetaValue, TempMode, Whence, temp_stream};
use crate::api::raising::Thrown;
use crate::api::request::{Request, Stream};
use crate::api::values::Owner;
use crate::streams::StreamLayer;

/// What the opener answered: the stream, or the text php's wrapper logs,
/// which `php_stream_display_wrapper_errors` prints after `Failed to open
/// stream: `.
type Opened = Result<Owner<Stream>, String>;

/// `php_stream_url_wrap_rfc2397`: the path after `data:` and the `//`
/// php lets stand after it split at its first comma; before the comma a
/// media type with a `/` in it, then `;name=value` parameters, a
/// `mediatype` one dropped, and last `;base64`, or `;base64` alone,
/// anything else php's `illegal media type`, `illegal parameter` or
/// `illegal URL`; after it the data, base64-decoded strictly or
/// URL-decoded. The stream is a temp one that never spills, the data
/// written and the stream back at its start, `mode` as given.
///
/// ```php
/// foreach (['data:,A%20brief%20note', 'data://text/plain;charset=utf-8;base64,SGVsbG8=', 'data:text/plain;a=1;a=2;mediatype=x,z'] as $url) {
///     $f = fopen($url, 'r');
///     var_dump(stream_get_contents($f));
///     echo json_encode(array_slice(stream_get_meta_data($f), 0, -3)), "\n";
/// }
/// foreach (['data://', 'data:foo,', 'data:text/plain;bad,', 'data:;base64,@@', 'data:text/plain;a=b;c'] as $url) var_dump(@file_get_contents($url), error_get_last()['message']);
/// ```
/// ```expect
/// string(12) "A brief note"
/// {"base64":false,"wrapper_type":"RFC2397","stream_type":"RFC2397","mode":"r"}
/// string(5) "Hello"
/// {"mediatype":"text\/plain","charset":"utf-8","base64":true,"wrapper_type":"RFC2397","stream_type":"RFC2397","mode":"r"}
/// string(1) "z"
/// {"mediatype":"text\/plain","a":"2","base64":false,"wrapper_type":"RFC2397","stream_type":"RFC2397","mode":"r"}
/// bool(false)
/// string(75) "file_get_contents(data://): Failed to open stream: rfc2397: no comma in URL"
/// bool(false)
/// string(80) "file_get_contents(data:foo,): Failed to open stream: rfc2397: illegal media type"
/// bool(false)
/// string(90) "file_get_contents(data:text/plain;bad,): Failed to open stream: rfc2397: illegal parameter"
/// bool(false)
/// string(84) "file_get_contents(data:;base64,@@): Failed to open stream: rfc2397: unable to decode"
/// bool(false)
/// string(89) "file_get_contents(data:text/plain;a=b;c): Failed to open stream: rfc2397: no comma in URL"
/// ```
pub(super) fn open(req: &mut Request, path: &[u8], mode: &[u8]) -> Result<Opened, Thrown> {
    let Some(rest) = path.strip_prefix(b"data:") else {
        return Ok(Err(String::from("operation failed")));
    };
    let rest = rest.strip_prefix(b"//").unwrap_or(rest);
    let (meta, base64, data) = match parse(rest) {
        Ok(parsed) => parsed,
        Err(message) => return Ok(Err(format!("rfc2397: {message}"))),
    };
    let data = if base64 {
        match base64_decode_strict(data) {
            Some(bytes) => bytes,
            None => return Ok(Err(String::from("rfc2397: unable to decode"))),
        }
    } else {
        url_decode(data)
    };
    let stream = temp_stream(req, TempMode::Default, usize::MAX, "RFC2397", meta)?;
    req.stream_op(*stream, |op| {
        op.kind(|req, ops| {
            let _ = ops.write(req, &data);
            let mut at = 0;
            let _ = ops.seek(req, 0, Some(Whence::Set), &mut at);
        });
        op.with(|s| s.mode = super::Mode::of(mode));
    });
    Ok(Ok(stream))
}

/// The meta data of a URL, in the order php adds it, a name given twice
/// keeping its first place and its last value, as `add_assoc_stringl_ex`
/// over one key does.
type Meta = Vec<(Vec<u8>, MetaValue)>;

fn add(meta: &mut Meta, name: &[u8], value: MetaValue) {
    match meta.iter_mut().find(|(key, _)| key == name) {
        Some(entry) => entry.1 = value,
        None => meta.push((name.to_vec(), value)),
    }
}

/// The part before the comma as php's opener walks it: the meta data,
/// whether the data is base64, and the data after the comma; the `Err`
/// is the log line's tail.
fn parse(path: &[u8]) -> Result<(Meta, bool, &[u8]), &'static str> {
    let find = |hay: &[u8], b: u8| hay.iter().position(|c| *c == b);
    let Some(comma) = find(path, b',') else {
        return Err("no comma in URL");
    };
    let mut meta = Meta::new();
    let mut base64 = false;
    if comma > 0 {
        let mut head = &path[..comma];
        let semi = find(head, b';');
        let sep = find(head, b'/');
        match (semi, sep) {
            (None, None) => return Err("illegal media type"),
            (None, Some(_)) => {
                add(&mut meta, b"mediatype", MetaValue::Str(head.to_vec()));
                head = &[];
            }
            (Some(semi), Some(sep)) if sep < semi => {
                add(
                    &mut meta,
                    b"mediatype",
                    MetaValue::Str(head[..semi].to_vec()),
                );
                head = &head[semi..];
            }
            (Some(semi), _) if semi != 0 || head != b";base64" => {
                return Err("illegal media type");
            }
            _ => {}
        }
        // Each `;name=value`, and a last `;base64`.
        while head.first() == Some(&b';') {
            head = &head[1..];
            let sep = find(head, b'=');
            let semi = find(head, b';');
            match sep {
                Some(eq) if semi.is_none_or(|semi| semi >= eq) => {
                    let value_end = semi.unwrap_or(head.len());
                    let name = &head[..eq];
                    if name != b"mediatype" {
                        add(
                            &mut meta,
                            name,
                            MetaValue::Str(head[eq + 1..value_end].to_vec()),
                        );
                    }
                    head = &head[value_end..];
                }
                _ => {
                    if head != b"base64" {
                        return Err("illegal parameter");
                    }
                    base64 = true;
                    head = &[];
                    break;
                }
            }
        }
        if !head.is_empty() {
            return Err("illegal URL");
        }
    }
    add(&mut meta, b"base64", MetaValue::Bool(base64));
    Ok((meta, base64, &path[comma + 1..]))
}

/// `php_url_decode`, the engine's one.
pub(super) fn url_decode(s: &[u8]) -> Vec<u8> {
    crate::api::request::url_decode(s, true)
}

/// `php_base64_decode_ex(str, len, 1)`, the strict decode: the four
/// white-space bytes skipped, any other byte outside the alphabet, a
/// digit after `=`, one digit left over, or padding that is more than
/// two or leaves the group short, refused.
fn base64_decode_strict(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut i = 0usize;
    let mut padding = 0usize;
    let mut pending = 0u8;
    for &byte in input {
        let ch = match byte {
            b'=' => {
                padding += 1;
                continue;
            }
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'\t' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        };
        if padding > 0 {
            return None;
        }
        match i % 4 {
            0 => pending = ch << 2,
            1 => {
                out.push(pending | (ch >> 4));
                pending = (ch & 0x0f) << 4;
            }
            2 => {
                out.push(pending | (ch >> 2));
                pending = (ch & 0x03) << 6;
            }
            _ => out.push(pending | ch),
        }
        i += 1;
    }
    if i % 4 == 1 || (padding > 0 && (padding > 2 || !(i + padding).is_multiple_of(4))) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{Meta, MetaValue, parse};

    /// The meta data as `name=value` in order, which `MetaValue` cannot
    /// show itself.
    fn shown(meta: &Meta) -> String {
        let pairs: Vec<String> = meta
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    MetaValue::Bool(flag) => flag.to_string(),
                    MetaValue::Int(number) => number.to_string(),
                    MetaValue::Str(text) => String::from_utf8_lossy(text).into_owned(),
                    MetaValue::Value(_) => String::from("(a request value)"),
                };
                format!("{}={value}", String::from_utf8_lossy(name))
            })
            .collect();
        pairs.join(" ")
    }

    #[test]
    fn the_part_before_the_comma_is_read_as_php_reads_it() {
        type Parsed<'a> = Result<(&'a str, bool, &'a [u8]), &'a str>;
        let table: &[(&[u8], Parsed<'_>)] = &[
            (b",A%20brief", Ok(("base64=false", false, b"A%20brief"))),
            (b",a,b", Ok(("base64=false", false, b"a,b"))),
            (
                b"text/plain,x",
                Ok(("mediatype=text/plain base64=false", false, b"x")),
            ),
            (b"/,x", Ok(("mediatype=/ base64=false", false, b"x"))),
            (b";base64,SGk=", Ok(("base64=true", true, b"SGk="))),
            (
                b"text/plain;charset=utf-8;base64,SGVsbG8=",
                Ok((
                    "mediatype=text/plain charset=utf-8 base64=true",
                    true,
                    b"SGVsbG8=",
                )),
            ),
            (
                b"text/plain;a=1;b=3;a=2,z",
                Ok(("mediatype=text/plain a=2 b=3 base64=false", false, b"z")),
            ),
            (
                b"text/plain;mediatype=x,z",
                Ok(("mediatype=text/plain base64=false", false, b"z")),
            ),
            (
                b"text/plain;a=;b=c=d,z",
                Ok(("mediatype=text/plain a= b=c=d base64=false", false, b"z")),
            ),
            (b"", Err("no comma in URL")),
            (b"text/plain;a=b;c", Err("no comma in URL")),
            (b"foo,", Err("illegal media type")),
            (b";a=b,x", Err("illegal media type")),
            (b";base64;a=b,x", Err("illegal media type")),
            (b"text;a=b/c,x", Err("illegal media type")),
            (b"text/plain;bad,", Err("illegal parameter")),
            (b"text/plain;a=b;c,", Err("illegal parameter")),
            (b"text/plain;base64x,", Err("illegal parameter")),
            (b"text/plain;base64;a=b,", Err("illegal parameter")),
        ];
        for (path, expected) in table {
            let parsed = parse(path).map(|(meta, base64, data)| (shown(&meta), base64, data));
            let expected = expected.map(|(meta, base64, data)| (meta.to_owned(), base64, data));
            assert_eq!(parsed, expected, "{}", String::from_utf8_lossy(path));
        }
    }
}

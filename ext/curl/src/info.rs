//! `curl_getinfo`: one piece of what the last transfer left, by its
//! `CURLINFO_*` number, or all of it as php's array.

use php_ext::abi::Instance;
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::constants as c;
use crate::ffi::{EasyId, Info};
use crate::handle::{CurlHandle, curl, easy_of};

/// An array of the request's being filled with `add_assoc_*`.
pub(crate) struct Assoc(pub(crate) Owner<PhpArray>);

impl Assoc {
    pub(crate) fn new(req: &mut Request) -> Result<Assoc, Thrown> {
        PhpArray::new(req).map(Assoc)
    }

    /// `add_assoc_zval`: `value` is the array's from here.
    pub(crate) fn add(
        &mut self,
        req: &mut Request,
        key: &str,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let key = match PhpString::new(req, key.as_bytes()) {
            Ok(key) => key.into_key(),
            Err(thrown) => {
                value.give_up(req);
                return Err(thrown);
            }
        };
        self.0.set(req, key, value)
    }

    /// `add_assoc_string`.
    pub(crate) fn add_str(
        &mut self,
        req: &mut Request,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), Thrown> {
        let value = PhpString::new(req, bytes)?.into_value();
        self.add(req, key, value)
    }

    pub(crate) fn value(self) -> Owner<Value> {
        self.0.into_value()
    }
}

/// A list of strings, `add_next_index_string` each.
pub(crate) fn list(req: &mut Request, items: &[Vec<u8>]) -> Result<Owner<Value>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for item in items {
        let value = PhpString::new(req, item)?.into_value();
        array.append(req, value)?;
    }
    Ok(array.into_value())
}

/// `create_certinfo`: each certificate an array of its `key:value`
/// lines, the key cut at the first colon of the line's first 63 bytes.
fn certinfo(
    req: &mut Request,
    certs: &[Vec<Vec<u8>>],
    into: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    for cert in certs {
        let mut hash = Assoc::new(req)?;
        for line in cert {
            let head = &line[..line.len().min(63)];
            match head.iter().position(|b| *b == b':') {
                Some(at) => {
                    let key = String::from_utf8_lossy(&line[..at]).into_owned();
                    hash.add_str(req, &key, &line[at + 1..])?;
                }
                None => docref(
                    req,
                    Level::Warning,
                    "Could not extract hash key from certificate info",
                ),
            }
        }
        into.append(req, hash.value())?;
    }
    Ok(())
}

/// What the whole-array form reads each row as.
#[derive(Clone, Copy)]
enum Row {
    /// `CAAS`: a string, `""` for none.
    Str,
    /// `content_type`: a string, `null` for none.
    OrNull,
    /// `CAAL` or `CAAD`: the number as the info's type gives it.
    Number,
}

/// The rows of `curl_getinfo($ch)` before `certinfo`, in php's order.
const HEAD: &[(&str, i64, Row)] = &[
    ("url", c::CURLINFO_EFFECTIVE_URL, Row::Str),
    ("content_type", c::CURLINFO_CONTENT_TYPE, Row::OrNull),
    ("http_code", c::CURLINFO_HTTP_CODE, Row::Number),
    ("header_size", c::CURLINFO_HEADER_SIZE, Row::Number),
    ("request_size", c::CURLINFO_REQUEST_SIZE, Row::Number),
    ("filetime", c::CURLINFO_FILETIME, Row::Number),
    (
        "ssl_verify_result",
        c::CURLINFO_SSL_VERIFYRESULT,
        Row::Number,
    ),
    ("redirect_count", c::CURLINFO_REDIRECT_COUNT, Row::Number),
    ("total_time", c::CURLINFO_TOTAL_TIME, Row::Number),
    ("namelookup_time", c::CURLINFO_NAMELOOKUP_TIME, Row::Number),
    ("connect_time", c::CURLINFO_CONNECT_TIME, Row::Number),
    (
        "pretransfer_time",
        c::CURLINFO_PRETRANSFER_TIME,
        Row::Number,
    ),
    ("size_upload", c::CURLINFO_SIZE_UPLOAD, Row::Number),
    ("size_download", c::CURLINFO_SIZE_DOWNLOAD, Row::Number),
    ("speed_download", c::CURLINFO_SPEED_DOWNLOAD, Row::Number),
    ("speed_upload", c::CURLINFO_SPEED_UPLOAD, Row::Number),
    (
        "download_content_length",
        c::CURLINFO_CONTENT_LENGTH_DOWNLOAD,
        Row::Number,
    ),
    (
        "upload_content_length",
        c::CURLINFO_CONTENT_LENGTH_UPLOAD,
        Row::Number,
    ),
    (
        "starttransfer_time",
        c::CURLINFO_STARTTRANSFER_TIME,
        Row::Number,
    ),
    ("redirect_time", c::CURLINFO_REDIRECT_TIME, Row::Number),
    ("redirect_url", c::CURLINFO_REDIRECT_URL, Row::Str),
    ("primary_ip", c::CURLINFO_PRIMARY_IP, Row::Str),
];

/// The rows after `certinfo` and before `request_header`.
const MIDDLE: &[(&str, i64, Row)] = &[
    ("primary_port", c::CURLINFO_PRIMARY_PORT, Row::Number),
    ("local_ip", c::CURLINFO_LOCAL_IP, Row::Str),
    ("local_port", c::CURLINFO_LOCAL_PORT, Row::Number),
    ("http_version", c::CURLINFO_HTTP_VERSION, Row::Number),
    ("protocol", c::CURLINFO_PROTOCOL, Row::Number),
    (
        "ssl_verifyresult",
        c::CURLINFO_PROXY_SSL_VERIFYRESULT,
        Row::Number,
    ),
    ("scheme", c::CURLINFO_SCHEME, Row::Str),
    (
        "appconnect_time_us",
        c::CURLINFO_APPCONNECT_TIME_T,
        Row::Number,
    ),
    ("queue_time_us", c::CURLINFO_QUEUE_TIME_T, Row::Number),
    ("connect_time_us", c::CURLINFO_CONNECT_TIME_T, Row::Number),
    (
        "namelookup_time_us",
        c::CURLINFO_NAMELOOKUP_TIME_T,
        Row::Number,
    ),
    (
        "pretransfer_time_us",
        c::CURLINFO_PRETRANSFER_TIME_T,
        Row::Number,
    ),
    ("redirect_time_us", c::CURLINFO_REDIRECT_TIME_T, Row::Number),
    (
        "starttransfer_time_us",
        c::CURLINFO_STARTTRANSFER_TIME_T,
        Row::Number,
    ),
    (
        "posttransfer_time_us",
        c::CURLINFO_POSTTRANSFER_TIME_T,
        Row::Number,
    ),
    ("total_time_us", c::CURLINFO_TOTAL_TIME_T, Row::Number),
];

/// The rows after `request_header`.
const TAIL: &[(&str, i64, Row)] = &[
    ("effective_method", c::CURLINFO_EFFECTIVE_METHOD, Row::Str),
    ("capath", c::CURLINFO_CAPATH, Row::Str),
    ("cainfo", c::CURLINFO_CAINFO, Row::Str),
    ("used_proxy", c::CURLINFO_USED_PROXY, Row::Number),
    ("httpauth_used", c::CURLINFO_HTTPAUTH_USED, Row::Number),
    ("proxyauth_used", c::CURLINFO_PROXYAUTH_USED, Row::Number),
    ("conn_id", c::CURLINFO_CONN_ID, Row::Number),
];

/// Each row the library answers, added as `row` reads it.
fn rows(
    req: &mut Request,
    easy: EasyId,
    out: &mut Assoc,
    rows: &[(&str, i64, Row)],
) -> Result<(), Thrown> {
    for (key, info, row) in rows {
        let value = match (easy.info(req, *info), row) {
            (Some(Info::Str(None)), Row::OrNull) => Owner::take(req, Value::null()),
            (Some(Info::Str(text)), _) => {
                PhpString::new(req, &text.unwrap_or_default())?.into_value()
            }
            (Some(Info::Long(n) | Info::OffT(n)), _) => Owner::take(req, Value::int(n)),
            (Some(Info::Double(f)), _) => Owner::take(req, Value::float(f)),
            (None | Some(Info::List(_) | Info::Certs(_)), _) => continue,
        };
        out.add(req, key, value)?;
    }
    Ok(())
}

/// `curl_getinfo(CurlHandle $handle, ?int $option = null): mixed`.
///
/// ```php
/// $c = curl_init('file:///nonexistent-curl-witness');
/// $i = curl_getinfo($c);
/// var_dump(count($i), $i['url'], $i['content_type'], $i['http_code'], array_key_exists('request_header', $i));
/// var_dump(curl_getinfo($c, CURLINFO_HEADER_OUT), curl_getinfo($c, CURLINFO_PRIVATE), curl_getinfo($c, CURLINFO_CERTINFO), curl_getinfo($c, CURLINFO_RESPONSE_CODE), curl_getinfo($c, 12345));
/// ```
/// ```expect
/// int(46)
/// string(32) "file:///nonexistent-curl-witness"
/// NULL
/// int(0)
/// bool(false)
/// bool(false)
/// bool(false)
/// array(0) {
/// }
/// int(0)
/// bool(false)
/// ```
#[php_function]
fn curl_getinfo(
    req: &mut Request,
    handle: Instance<CurlHandle>,
    #[php(default = null)] option: Option<i64>,
) -> Result<Owner<Value>, Thrown> {
    let object = handle.object();
    let easy = easy_of(req, object)?;
    let Some(option) = option else {
        let mut out = Assoc::new(req)?;
        rows(req, easy, &mut out, HEAD)?;
        if let Some(Info::Certs(certs)) = easy.info(req, c::CURLINFO_CERTINFO) {
            let mut array = PhpArray::new(req)?;
            certinfo(req, &certs, &mut array)?;
            out.add(req, "certinfo", array.into_value())?;
        }
        rows(req, easy, &mut out, MIDDLE)?;
        if let Some(header) = curl(req, handle)?.header_out.clone() {
            out.add_str(req, "request_header", &header)?;
        }
        rows(req, easy, &mut out, TAIL)?;
        return Ok(out.value());
    };
    let value = match option {
        c::CURLINFO_HEADER_OUT => match curl(req, handle)?.header_out.clone() {
            Some(header) => PhpString::new(req, &header)?.into_value(),
            None => Owner::take(req, Value::bool(false)),
        },
        c::CURLINFO_PRIVATE => match curl(req, handle)?.private {
            Some(private) => Owner::take(req, private),
            None => Owner::take(req, Value::bool(false)),
        },
        _ => match easy.info(req, option) {
            Some(Info::Str(Some(text))) => PhpString::new(req, &text)?.into_value(),
            Some(Info::Long(n) | Info::OffT(n)) => Owner::take(req, Value::int(n)),
            Some(Info::Double(f)) => Owner::take(req, Value::float(f)),
            Some(Info::List(lines)) => list(req, &lines)?,
            Some(Info::Certs(certs)) => {
                let mut array = PhpArray::new(req)?;
                certinfo(req, &certs, &mut array)?;
                array.into_value()
            }
            Some(Info::Str(None)) | None => Owner::take(req, Value::bool(false)),
        },
    };
    Ok(value)
}

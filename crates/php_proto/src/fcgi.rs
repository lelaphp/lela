//! The `FastCGI` record protocol: the binary wire format a web server speaks
//! to a `FastCGI` application — fixed 8-byte record headers framing typed
//! content (`FCGI_BEGIN_REQUEST`, `FCGI_PARAMS`, `FCGI_STDIN`,
//! `FCGI_STDOUT`, `FCGI_END_REQUEST`, …), plus the `PARAMS` name-value pair
//! encoding. Bytes alone — no socket, no engine — so it tests in process.
//! Faithful to the `FastCGI` 1.0 specification.

use std::io::{self, Read, Write};

/// The `FastCGI` protocol version byte (`FCGI_VERSION_1`).
pub const VERSION_1: u8 = 1;

/// Record types (`FCGI_*`).
pub mod rtype {
    /// `FCGI_BEGIN_REQUEST` — opens a request (carries the role + flags).
    pub const BEGIN_REQUEST: u8 = 1;
    /// `FCGI_ABORT_REQUEST` — the server abandons a request.
    pub const ABORT_REQUEST: u8 = 2;
    /// `FCGI_END_REQUEST` — the app closes a request (app status + protocol status).
    pub const END_REQUEST: u8 = 3;
    /// `FCGI_PARAMS` — a stream of name-value pairs (the CGI environment).
    pub const PARAMS: u8 = 4;
    /// `FCGI_STDIN` — the request body stream.
    pub const STDIN: u8 = 5;
    /// `FCGI_STDOUT` — the response body stream (app → server).
    pub const STDOUT: u8 = 6;
    /// `FCGI_STDERR` — the response error stream (app → server).
    pub const STDERR: u8 = 7;
    /// `FCGI_GET_VALUES` — the server asks what the app supports.
    pub const GET_VALUES: u8 = 9;
    /// `FCGI_GET_VALUES_RESULT` — the app's answer to `FCGI_GET_VALUES`.
    pub const GET_VALUES_REPLY: u8 = 10;
    /// `FCGI_UNKNOWN_TYPE` — the app rejects an unknown management record.
    pub const UNKNOWN_TYPE: u8 = 11;
}

/// The `FCGI_RESPONDER` role (the only role this engine serves).
pub const ROLE_RESPONDER: u16 = 1;
/// `FCGI_KEEP_CONN` (in `BEGIN_REQUEST`): keep the connection open after the
/// request completes.
pub const KEEP_CONN: u8 = 1;
/// `FCGI_REQUEST_COMPLETE` protocol status (in `END_REQUEST`).
pub const REQUEST_COMPLETE: u8 = 0;

/// One decoded `FastCGI` record.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    /// The record type (`rtype::*`).
    pub rtype: u8,
    /// The request id this record belongs to (management records use 0).
    pub request_id: u16,
    /// The record's content (padding stripped).
    pub content: Vec<u8>,
}

/// Read one record from `r`, or `None` at a clean EOF before any header.
///
/// # Errors
///
/// An [`io::Error`] on a partial/short read or an I/O failure.
pub fn read_record(r: &mut impl Read) -> io::Result<Option<Record>> {
    let mut header = [0u8; 8];
    if !read_exact_or_eof(r, &mut header)? {
        return Ok(None);
    }
    let rtype = header[1];
    let request_id = u16::from_be_bytes([header[2], header[3]]);
    let content_len = usize::from(u16::from_be_bytes([header[4], header[5]]));
    let padding_len = usize::from(header[6]);
    let mut content = vec![0u8; content_len];
    r.read_exact(&mut content)?;
    if padding_len > 0 {
        let mut pad = vec![0u8; padding_len];
        r.read_exact(&mut pad)?;
    }
    Ok(Some(Record {
        rtype,
        request_id,
        content,
    }))
}

/// Write one record (no padding) to `w`. Content longer than 65535 bytes is
/// split into successive records of the same type/id by [`write_stream`],
/// which is how a body of any length is sent.
///
/// # Errors
///
/// An [`io::Error`] on an I/O failure, and [`io::ErrorKind::InvalidInput`]
/// when `content` is longer than the 65535-byte content-length ceiling.
pub fn write_record(
    w: &mut impl Write,
    rtype: u8,
    request_id: u16,
    content: &[u8],
) -> io::Result<()> {
    let Ok(len) = u16::try_from(content.len()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "record content > 65535 bytes; send it through write_stream",
        ));
    };
    let rid = request_id.to_be_bytes();
    let clen = len.to_be_bytes();
    let header = [VERSION_1, rtype, rid[0], rid[1], clen[0], clen[1], 0, 0];
    w.write_all(&header)?;
    w.write_all(content)
}

/// Write a (possibly large) stream as one-or-more records of `rtype`/`id`,
/// each at most 65535 bytes (the `FastCGI` content-length ceiling).
///
/// # Errors
///
/// An [`io::Error`] on an I/O failure.
pub fn write_stream(w: &mut impl Write, rtype: u8, request_id: u16, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return write_record(w, rtype, request_id, &[]);
    }
    for chunk in data.chunks(65535) {
        write_record(w, rtype, request_id, chunk)?;
    }
    Ok(())
}

/// Write an `FCGI_END_REQUEST` record (app status + protocol status).
///
/// # Errors
///
/// An [`io::Error`] on an I/O failure.
pub fn write_end_request(
    w: &mut impl Write,
    request_id: u16,
    app_status: u32,
    protocol_status: u8,
) -> io::Result<()> {
    let s = app_status.to_be_bytes();
    let body = [s[0], s[1], s[2], s[3], protocol_status, 0, 0, 0];
    write_record(w, rtype::END_REQUEST, request_id, &body)
}

/// The role from a `BEGIN_REQUEST` body (`content[0..2]` big-endian), and the
/// flags byte (`content[2]`). Defaults to responder/0 on a short body.
#[must_use]
pub fn begin_request_role(content: &[u8]) -> (u16, u8) {
    if content.len() >= 3 {
        (u16::from_be_bytes([content[0], content[1]]), content[2])
    } else {
        (ROLE_RESPONDER, 0)
    }
}

/// Decode a `FCGI_PARAMS` name-value-pair stream into `(name, value)` byte
/// pairs. Each pair is `nameLen valueLen nameData valueData`; a length is one
/// byte when its high bit is clear, else a 4-byte big-endian value with the
/// high bit of the first byte masked off.
#[must_use]
pub fn parse_name_value_pairs(mut data: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let Some(name_len) = take_len(&mut data) else {
            break;
        };
        let Some(value_len) = take_len(&mut data) else {
            break;
        };
        if data.len() < name_len + value_len {
            break;
        }
        let (name, rest) = data.split_at(name_len);
        let (value, rest) = rest.split_at(value_len);
        out.push((name.to_vec(), value.to_vec()));
        data = rest;
    }
    out
}

/// Build the `FCGI_GET_VALUES_RESULT` name-value payload answering a
/// `FCGI_GET_VALUES` `query`. The query is a name-value-pair stream whose
/// values are empty; only the variables it names are answered
/// (`FCGI_MAX_CONNS`, `FCGI_MAX_REQS`, `FCGI_MPXS_CONNS`). An empty query
/// (no names) answers all three. One process serves one request at a time
/// and does not multiplex, so the answer is one connection, one request and
/// no multiplexing.
#[must_use]
pub fn get_values_reply(query: &[u8]) -> Vec<u8> {
    let asked = parse_name_value_pairs(query);
    let mut out = Vec::new();
    for &(name, value) in ANSWERS {
        let wanted = asked.is_empty() || asked.iter().any(|(n, _)| n.as_slice() == name);
        if wanted {
            encode_name_value(&mut out, name, value);
        }
    }
    out
}

/// What this application says of itself, the three variables a server asks
/// about (single-process, no mux). Every name and value is under 128 bytes,
/// which is what lets [`encode_name_value`] write the one-byte length form;
/// `answers_fit_the_one_byte_length_form` holds that.
const ANSWERS: &[(&[u8], &[u8])] = &[
    (b"FCGI_MAX_CONNS", b"1"),
    (b"FCGI_MAX_REQS", b"1"),
    (b"FCGI_MPXS_CONNS", b"0"),
];

/// Append one `FastCGI` name-value pair to `out` in the one-byte length
/// form, the inverse of one step of [`parse_name_value_pairs`] for a name
/// and a value each under 128 bytes. [`ANSWERS`] is the only source of a
/// pair here and every one of them is; a longer one is left out rather than
/// written with a length the reader would take for [`take_len`]'s four-byte
/// form, which is the form this would grow if a caller ever needed it.
fn encode_name_value(out: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    let (Ok(nl), Ok(vl)) = (u8::try_from(name.len()), u8::try_from(value.len())) else {
        return;
    };
    if nl & 0x80 != 0 || vl & 0x80 != 0 {
        return;
    }
    out.push(nl);
    out.push(vl);
    out.extend_from_slice(name);
    out.extend_from_slice(value);
}

/// Consume a `FastCGI` length prefix (1 or 4 bytes) from `data`.
fn take_len(data: &mut &[u8]) -> Option<usize> {
    let first = *data.first()?;
    if first & 0x80 == 0 {
        *data = &data[1..];
        Some(usize::from(first))
    } else {
        if data.len() < 4 {
            return None;
        }
        let b = [data[0] & 0x7f, data[1], data[2], data[3]];
        *data = &data[4..];
        usize::try_from(u32::from_be_bytes(b)).ok()
    }
}

/// Read exactly `buf.len()` bytes; `Ok(false)` if EOF hits before the first
/// byte (a clean stream end), `Ok(true)` on a full read.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => return Err(io::ErrorKind::UnexpectedEof.into()),
            n => filled += n,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_a_buffer() {
        let mut buf = Vec::new();
        write_record(&mut buf, rtype::STDOUT, 7, b"hello world").unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let rec = read_record(&mut cur).unwrap().unwrap();
        assert_eq!(rec.rtype, rtype::STDOUT);
        assert_eq!(rec.request_id, 7);
        assert_eq!(rec.content, b"hello world");
        // A second read hits clean EOF.
        assert_eq!(read_record(&mut cur).unwrap(), None);
    }

    #[test]
    fn name_value_pairs_short_and_long_lengths() {
        // Two pairs: a short one, and a long-value one (>127 → 4-byte length).
        let big = vec![b'x'; 200];
        let mut params = Vec::new();
        // "A"="1" (short lengths)
        params.extend_from_slice(&[1, 1]);
        params.extend_from_slice(b"A1");
        // "K" = big (1-byte name len, 4-byte value len)
        params.push(1);
        let vl = u32::try_from(big.len()).unwrap() | 0x8000_0000;
        params.extend_from_slice(&vl.to_be_bytes());
        params.push(b'K');
        params.extend_from_slice(&big);
        let pairs = parse_name_value_pairs(&params);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (b"A".to_vec(), b"1".to_vec()));
        assert_eq!(pairs[1].0, b"K");
        assert_eq!(pairs[1].1, big);
    }

    #[test]
    fn begin_request_role_decodes_responder() {
        let role = ROLE_RESPONDER.to_be_bytes();
        let body = [role[0], role[1], KEEP_CONN, 0, 0, 0, 0, 0];
        assert_eq!(begin_request_role(&body), (ROLE_RESPONDER, KEEP_CONN));
    }

    #[test]
    fn write_stream_chunks_large_payloads() {
        let mut buf = Vec::new();
        let data = vec![b'z'; 70000]; // > 65535 → 2 records
        write_stream(&mut buf, rtype::STDOUT, 1, &data).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let r1 = read_record(&mut cur).unwrap().unwrap();
        let r2 = read_record(&mut cur).unwrap().unwrap();
        assert_eq!(r1.content.len(), 65535);
        assert_eq!(r2.content.len(), 70000 - 65535);
    }

    #[test]
    fn get_values_reply_answers_only_the_variables_asked_for() {
        // The query asks for two of the three variables, each with an empty
        // value (as a server does).
        let mut query = Vec::new();
        for name in [b"FCGI_MAX_CONNS".as_slice(), b"FCGI_MPXS_CONNS".as_slice()] {
            query.push(u8::try_from(name.len()).unwrap());
            query.push(0u8);
            query.extend_from_slice(name);
        }
        let pairs = parse_name_value_pairs(&get_values_reply(&query));
        assert_eq!(pairs.len(), 2);
        assert!(
            pairs
                .iter()
                .any(|(n, v)| n == b"FCGI_MAX_CONNS" && v == b"1")
        );
        assert!(
            pairs
                .iter()
                .any(|(n, v)| n == b"FCGI_MPXS_CONNS" && v == b"0")
        );
        assert!(!pairs.iter().any(|(n, _)| n == b"FCGI_MAX_REQS"));
    }

    #[test]
    fn get_values_reply_empty_query_answers_all_three() {
        let pairs = parse_name_value_pairs(&get_values_reply(&[]));
        assert_eq!(pairs.len(), 3);
    }

    #[test]
    fn end_request_encodes_status() {
        let mut buf = Vec::new();
        write_end_request(&mut buf, 3, 0, REQUEST_COMPLETE).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let rec = read_record(&mut cur).unwrap().unwrap();
        assert_eq!(rec.rtype, rtype::END_REQUEST);
        assert_eq!(rec.request_id, 3);
        assert_eq!(rec.content, vec![0, 0, 0, 0, REQUEST_COMPLETE, 0, 0, 0]);
    }

    #[test]
    fn write_record_refuses_content_over_the_ceiling() {
        let mut buf = Vec::new();
        let too_big = vec![b'z'; 65536];
        let err = write_record(&mut buf, rtype::STDOUT, 1, &too_big).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(buf.is_empty());
    }

    #[test]
    fn answers_fit_the_one_byte_length_form() {
        // What makes `encode_name_value`'s one-byte form sound: no name or
        // value it is ever called with reaches the 128 that would set the
        // high bit and make `take_len` read four bytes.
        for (name, value) in ANSWERS {
            assert!(name.len() < 0x80, "{}", String::from_utf8_lossy(name));
            assert!(value.len() < 0x80, "{}", String::from_utf8_lossy(value));
        }
    }
}

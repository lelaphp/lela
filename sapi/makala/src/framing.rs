//! RFC 9112's framing rules that hyper's HTTP/1.1 server leaves to the
//! host (#2340), refused from the head it parsed before a request reaches
//! the request side.
//!
//! hyper's parser refuses with 400 and a close, before a `Service` sees
//! the request: a `Content-Length` that is not all digits, two that
//! differ, and one past `u64`; `chunked` not the last transfer coding;
//! `Transfer-Encoding` on HTTP/1.0; a folded field, a bare CR, and
//! whitespace before a colon. Where `Transfer-Encoding` and
//! `Content-Length` come together it drops the length, reads the body by
//! the coding and closes after the response (§6.1, §6.3 rule 3), so the
//! bytes a length would have framed are never read as a request. Its
//! chunked decoder holds each chunk's lines to CRLF and refuses a size
//! past `u64`. What it parses and lets through, the rules here refuse: a
//! request's `Host` (§3.2) and a transfer coding it would pass on
//! undecoded (§6.1). A body that breaks, and one that ends in a trailer
//! section, which hyper reads leniently, are refused as they are read
//! (`connection::BodyFault`).
//!
//! Two of the RFC's choices hyper makes for the host, with no option to
//! make them otherwise: a request with both `Transfer-Encoding` and
//! `Content-Length` is read by the coding and closed, never refused,
//! since the length is gone before a `Service` sees the head; and a bare LF
//! ending a line of the head is taken as its end (§2.2), which the RFC
//! lets a recipient do and which no configuration of its parser refuses.
//! One `Content-Length` sent twice alike is read as one (§6.3 rule 5).

use hyper::StatusCode;
use hyper::header::{HOST, TRANSFER_ENCODING};
use hyper::http::request::Parts;

/// Why a request's head is refused, or `None` where its framing holds.
pub(crate) fn refusal(parts: &Parts) -> Option<StatusCode> {
    host_refused(parts).or_else(|| coding_refused(parts))
}

/// §3.2: a server answers 400 to an HTTP/1.1 request with no `Host`, to
/// any request with more than one, and to one whose value is not a host
/// and `port`; an empty value is the one a target with no authority sends.
fn host_refused(parts: &Parts) -> Option<StatusCode> {
    let mut hosts = parts.headers.get_all(HOST).iter();
    let refused = match (hosts.next(), hosts.next()) {
        (None, _) => parts.version != hyper::Version::HTTP_10,
        (Some(host), None) => !is_host(host.as_bytes()),
        (Some(_), Some(_)) => true,
    };
    refused.then_some(StatusCode::BAD_REQUEST)
}

/// `uri-host [ ":" port ]` (RFC 3986 §3.2.2, §3.2.3): an `IP-literal` in
/// brackets or a registered name of unreserved, sub-delimiting and
/// percent-encoded bytes, then digits after a colon.
fn is_host(value: &[u8]) -> bool {
    let (host, port) = match value.strip_prefix(b"[") {
        Some(literal) => match literal.iter().position(|b| *b == b']') {
            Some(end) if literal[..end].iter().all(|b| is_literal_byte(*b)) => {
                (&b""[..], &literal[end + 1..])
            }
            _ => return false,
        },
        None => match value.iter().position(|b| *b == b':') {
            Some(colon) => (&value[..colon], &value[colon..]),
            None => (value, &b""[..]),
        },
    };
    is_reg_name(host)
        && match port.strip_prefix(b":") {
            Some(digits) => digits.iter().all(u8::is_ascii_digit),
            None => port.is_empty(),
        }
}

/// What an IPv6 address or an `IPvFuture` holds inside its brackets.
fn is_literal_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b":.-_~!$&'()*+,;=".contains(&b)
}

/// `reg-name`, which an IPv4 address also reads as.
fn is_reg_name(name: &[u8]) -> bool {
    let mut bytes = name.iter();
    while let Some(b) = bytes.next() {
        let held = match b {
            b'%' => {
                bytes.next().is_some_and(u8::is_ascii_hexdigit)
                    && bytes.next().is_some_and(u8::is_ascii_hexdigit)
            }
            b => b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=".contains(b),
        };
        if !held {
            return false;
        }
    }
    true
}

/// §6.1: the one transfer coding this server decodes is `chunked`, once.
/// hyper has refused a request whose last coding is not `chunked`; one
/// that names another before it is answered 501, which the RFC asks for
/// a coding not understood, since its body would reach the program still
/// coded; and `chunked` named twice, which no sender may do, is faulty
/// framing and answered 400.
fn coding_refused(parts: &Parts) -> Option<StatusCode> {
    let codings = parts
        .headers
        .get_all(TRANSFER_ENCODING)
        .iter()
        .flat_map(|value| value.as_bytes().split(|b| *b == b','))
        .map(<[u8]>::trim_ascii)
        .filter(|coding| !coding.is_empty());
    let mut chunked = 0;
    for coding in codings {
        if !coding.eq_ignore_ascii_case(b"chunked") {
            return Some(StatusCode::NOT_IMPLEMENTED);
        }
        chunked += 1;
    }
    (chunked > 1).then_some(StatusCode::BAD_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_a_name_or_a_literal_with_digits_after_a_colon() {
        for good in [
            &b""[..],
            b"a",
            b"a.example",
            b"a:80",
            b"a:",
            b"127.0.0.1:8080",
            b"[::1]",
            b"[::1]:80",
            b"%41b",
        ] {
            assert!(is_host(good), "{}", String::from_utf8_lossy(good));
        }
        for bad in [
            &b"a b"[..],
            b"a:x",
            b"u@a",
            b"[::1",
            b"[::1]x",
            b"a:80:80",
            b"%4",
            b"a/b",
        ] {
            assert!(!is_host(bad), "{}", String::from_utf8_lossy(bad));
        }
    }
}

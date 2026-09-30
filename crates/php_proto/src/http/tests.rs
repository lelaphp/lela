use super::*;

/// What the callbacks saw, as php's server keeps it.
#[derive(Default, Debug)]
struct Seen {
    method: Option<Method>,
    url: Vec<u8>,
    path: Vec<Vec<u8>>,
    query: Vec<u8>,
    fields: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
    body: Vec<u8>,
    version: Option<(u16, u16)>,
}

impl Events for Seen {
    fn path(&mut self, at: &[u8]) -> bool {
        self.path.push(at.to_vec());
        true
    }
    fn query_string(&mut self, at: &[u8]) -> bool {
        self.query.extend_from_slice(at);
        true
    }
    fn url(&mut self, method: Method, at: &[u8]) -> bool {
        self.method.get_or_insert(method);
        self.url.extend_from_slice(at);
        true
    }
    fn header_field(&mut self, at: &[u8]) -> bool {
        self.fields.push(at.to_vec());
        true
    }
    fn header_value(&mut self, at: &[u8]) -> bool {
        self.values.push(at.to_vec());
        true
    }
    fn headers_complete(&mut self) -> bool {
        true
    }
    fn body(&mut self, at: &[u8]) {
        self.body.extend_from_slice(at);
    }
    fn message_complete(&mut self, major: u16, minor: u16) -> bool {
        self.version = Some((major, minor));
        true
    }
}

fn parse(chunks: &[&[u8]]) -> (Seen, Vec<usize>) {
    let mut parser = Parser::new();
    let mut seen = Seen::default();
    let taken = chunks
        .iter()
        .map(|chunk| parser.execute(&mut seen, chunk))
        .collect();
    (seen, taken)
}

#[test]
fn a_request_with_bare_line_feeds_is_read_whole() {
    let request = b"GET /a/b?x=1#f HTTP/1.1\nHost:localhost\nUser-Agent: dummy\n\n";
    let (seen, taken) = parse(&[request]);
    assert_eq!(taken, [request.len()]);
    assert_eq!(seen.method, Some(Method::GET));
    assert_eq!(seen.url, b"/a/b?x=1#f");
    assert_eq!(seen.path, [b"/a/b".to_vec()]);
    assert_eq!(seen.query, b"x=1");
    assert_eq!(seen.fields, [b"Host".to_vec(), b"User-Agent".to_vec()]);
    assert_eq!(seen.values, [b"localhost".to_vec(), b"dummy".to_vec()]);
    assert_eq!(seen.version, Some((1, 1)));
}

#[test]
fn a_body_is_read_to_its_length_or_through_its_chunks() {
    let (seen, _) = parse(&[b"POST / HTTP/1.0\r\nContent-Length: 5\r\n\r\nhello"]);
    assert_eq!(
        (seen.method, seen.body.as_slice()),
        (Some(Method::POST), &b"hello"[..])
    );
    let (seen, _) = parse(&[
        b"PUT / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x\r\nde\r\n0\r\n\r\n",
    ]);
    assert_eq!(seen.body, b"abcde");
    assert_eq!(seen.version, Some((1, 1)));
}

#[test]
fn a_method_is_narrowed_as_its_bytes_come_and_one_php_knows_not_is_not_implemented() {
    let method = |line: &[u8]| parse(&[line]).0.method.map(Method::name);
    for name in [
        "MKCALENDAR",
        "MKCOL",
        "SEARCH",
        "PROPPATCH",
        "M-SEARCH",
        "PATCH",
        "CHECKOUT",
    ] {
        let line = format!("{name} / HTTP/1.1\r\n\r\n");
        assert_eq!(method(line.as_bytes()), Some(name));
    }
    for line in [
        &b"GETX / HTTP/1.1\r\n\r\n"[..],
        b"GE / HTTP/1.1\r\n\r\n",
        b"FOO / HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(method(line), Some("NOTIMPLEMENTED"));
    }
}

#[test]
fn a_header_split_over_two_buffers_is_handed_over_in_two_calls() {
    let (seen, taken) = parse(&[b"GET / HTTP/1.1\r\nHo", b"st: loc", b"alhost\r\n\r\n"]);
    assert_eq!(taken, [18, 7, 10]);
    assert_eq!(seen.fields, [b"Ho".to_vec(), b"st".to_vec()]);
    assert_eq!(seen.values, [b"loc".to_vec(), b"alhost".to_vec()]);
}

#[test]
fn what_php_refuses_stops_the_parse_where_it_stands() {
    assert_eq!(parse(&[b"get / HTTP/1.1\r\n\r\n"]).1, [0]);
    assert_eq!(parse(&[b"GET /\x01 HTTP/1.1\r\n\r\n"]).1, [5]);
    assert_eq!(
        parse(&[b"GET / HTTP/1.1\r\nBad Header\x01: x\r\n\r\n"]).1,
        [26]
    );
    let (_, taken) = parse(&[b"GET / HTTP/1.1\r\nContent-Length: 1x\r\n\r\n"]);
    assert_eq!(taken, [33]);
}

#[test]
fn the_path_is_decoded_and_its_dot_segments_taken_out() {
    assert_eq!(normalize_vpath(b"//a//b/./c/../d%20e"), b"/a/b/d e");
    assert_eq!(normalize_vpath(b"/../../x"), b"/x");
    assert_eq!(normalize_vpath(b"/a/.."), b"/");
    assert_eq!(normalize_vpath(b"/%2e%2e/%zz"), b"/%zz");
    assert_eq!(normalize_vpath(b"a/b"), b"a/b");
}

#[test]
fn the_reasons_are_phps_table() {
    assert_eq!(reason(404), Some("Not Found"));
    assert_eq!(reason(511), Some("Network Authentication Required"));
    assert_eq!(reason(422), None);
}

use std::io::{self, Write};

use super::*;
use crate::cgi::Environment;

/// Every record of `bytes`: its kind, id and content.
fn records(mut bytes: &[u8]) -> Vec<(Kind, u16, Vec<u8>)> {
    let mut out = Vec::new();
    while bytes.len() >= 8 {
        let header = Header::decode(bytes[..8].try_into().unwrap());
        let len = usize::from(header.content_length);
        out.push((header.kind, header.request_id, bytes[8..8 + len].to_vec()));
        bytes = &bytes[8 + len + usize::from(header.padding_length)..];
    }
    out
}

/// One record as a client writes it.
fn record(kind: Kind, id: u16, content: &[u8]) -> Vec<u8> {
    let header = Header::make(kind, id, content.len());
    let mut out = header.encode().to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + usize::from(header.padding_length), 0);
    out
}

/// A connection that reads `input` and keeps what is written.
struct Duplex {
    input: io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl io::Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_request_is_its_role_its_flags_and_its_params_in_the_order_set() {
    let mut pairs = Vec::new();
    put_pair(&mut pairs, b"A", b"1");
    put_pair(&mut pairs, b"B", &[b'x'; 200]);
    put_pair(&mut pairs, b"A", b"2");
    let mut input = record(Kind::Stdin, 1, b"");
    input.extend(record(Kind::BeginRequest, 7, &[0, 1, 1, 0, 0, 0, 0, 0]));
    input.extend(record(Kind::Params, 7, &pairs));
    input.extend(record(Kind::Params, 7, b""));
    input.extend(record(Kind::Stdin, 7, b"body"));
    input.extend(record(Kind::Stdin, 7, b""));
    let mut stream = Duplex {
        input: io::Cursor::new(input),
        output: Vec::new(),
    };
    let Read::Request(begun) = read_request(&mut stream, &[]) else {
        panic!("no request");
    };
    assert_eq!(
        (begun.id, begun.keep, begun.role),
        (7, true, Role::Responder)
    );
    let names: Vec<_> = begun.params.0.iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(names, [b"FCGI_ROLE".to_vec(), b"A".to_vec(), b"B".to_vec()]);
    assert_eq!(begun.params.get(b"A"), Some(&b"2"[..]));
    let newest: Vec<_> = newest_first(&begun.params)
        .map(|(n, _)| n.clone())
        .collect();
    assert_eq!(newest[0], b"B");
    let mut body = [0u8; 16];
    let mut input = Input::default();
    let n = input.read(&mut stream, &mut body).unwrap();
    assert_eq!(&body[..n], b"body");
    // The empty record that ends the body was read with it, as
    // `fcgi_read` reads it, so the stream holds nothing more.
    assert!(input.read(&mut stream, &mut body).is_err());
}

#[test]
fn get_values_is_answered_with_what_the_application_says_newest_first() {
    let mut pairs = Vec::new();
    put_pair(&mut pairs, b"FCGI_MPXS_CONNS", b"");
    put_pair(&mut pairs, b"NOPE", b"");
    put_pair(&mut pairs, b"FCGI_MAX_CONNS", b"");
    let mut stream = Duplex {
        input: io::Cursor::new(record(Kind::GetValues, 0, &pairs)),
        output: Vec::new(),
    };
    let values = [
        (b"FCGI_MAX_CONNS".to_vec(), b"5".to_vec()),
        (b"FCGI_MPXS_CONNS".to_vec(), b"0".to_vec()),
    ];
    assert_eq!(read_request(&mut stream, &values), Read::Answered);
    let got = records(&stream.output);
    assert_eq!(got[0].0, Kind::GetValuesResult);
    let mut answer = Environment::default();
    assert!(get_params(&mut answer, &got[0].2));
    assert_eq!(
        answer.0,
        [
            (b"FCGI_MAX_CONNS".to_vec(), b"5".to_vec()),
            (b"FCGI_MPXS_CONNS".to_vec(), b"0".to_vec())
        ]
    );
}

#[test]
fn a_record_that_opens_no_request_is_refused() {
    let mut stream = Duplex {
        input: io::Cursor::new(record(Kind::Stdout, 1, b"x")),
        output: Vec::new(),
    };
    assert_eq!(read_request(&mut stream, &[]), Read::Refused);
    let mut stream = Duplex {
        input: io::Cursor::new(record(Kind::BeginRequest, 1, &[0, 9, 0, 0, 0, 0, 0, 0])),
        output: Vec::new(),
    };
    assert_eq!(read_request(&mut stream, &[]), Read::Refused);
}

#[test]
fn output_is_gathered_padded_and_ended_once() {
    let mut wire = Vec::new();
    let mut out = Outgoing::new(3);
    out.write(&mut wire, Kind::Stdout, b"abc").unwrap();
    out.write(&mut wire, Kind::Stderr, b"e").unwrap();
    out.write(&mut wire, Kind::Stdout, b"d").unwrap();
    assert!(wire.is_empty());
    out.end(&mut wire).unwrap();
    out.end(&mut wire).unwrap();
    assert!(out.ended());
    assert_eq!(wire.len() % 8, 0);
    assert_eq!(
        records(&wire),
        [
            (Kind::Stdout, 3, b"abc".to_vec()),
            (Kind::Stderr, 3, b"e".to_vec()),
            (Kind::Stdout, 3, b"d".to_vec()),
            (Kind::EndRequest, 3, vec![0; 8]),
        ]
    );
}

#[test]
fn a_long_write_goes_in_records_no_longer_than_the_ceiling() {
    for size in [9000, 20_000, 200_000] {
        let mut wire = Vec::new();
        let mut out = Outgoing::new(1);
        let big: Vec<u8> = (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect();
        out.write(&mut wire, Kind::Stdout, b"head").unwrap();
        out.write(&mut wire, Kind::Stdout, &big).unwrap();
        out.flush(&mut wire, false).unwrap();
        let got = records(&wire);
        assert!(got.iter().all(|(_, _, c)| c.len() <= 0xffff));
        let joined: Vec<u8> = got.into_iter().flat_map(|(_, _, c)| c).collect();
        assert_eq!(&joined[..4], b"head");
        assert_eq!(&joined[4..], big.as_slice());
    }
}

#[test]
fn a_pair_running_past_the_end_is_malformed() {
    let mut params = Environment::default();
    assert!(!get_params(&mut params, &[5, 1, b'a']));
    assert!(!get_params(&mut params, &[0x80, 0]));
    assert!(get_params(&mut params, &[1, 0, b'a']));
    assert_eq!(params.get(b"a"), Some(&b""[..]));
}

/// A name of 130 bytes and a value of 200, each past the 127 a one-byte
/// length holds, are written with four-byte lengths, the high bit set, as
/// `fcgi_get_params` reads them, and read back whole (#3092).
#[test]
fn a_name_and_a_value_past_127_bytes_take_four_byte_lengths_both_ways() {
    let (name, value) = ([b'n'; 130], [b'v'; 200]);
    let mut pairs = Vec::new();
    put_pair(&mut pairs, &name, &value);
    assert_eq!(pairs[..8], [0x80, 0, 0, 130, 0x80, 0, 0, 200]);
    assert_eq!(pairs.len(), 8 + 130 + 200);
    let mut params = Environment::default();
    assert!(get_params(&mut params, &pairs));
    assert_eq!(params.0, [(name.to_vec(), value.to_vec())]);
}

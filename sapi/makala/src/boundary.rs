//! Where the bytes a connection's socket has handed hyper stand against the
//! requests they carry: in a head, in a body, or past the request in hand
//! (#2572). hyper reads ahead of the request it serves, so a pipelining
//! client's next head can wait in hyper's buffer, read with the request
//! before it, and the socket reads nothing more for it; the response's
//! end asks [`Boundary::ahead`] whether any byte of a next request has
//! been read, and starts the header clock rather than the keep-alive one
//! where one has.
//!
//! It frames nothing. hyper's reading is the one that serves, and this
//! one only follows it to say where each request ends, as RFC 9112 has
//! it: a head ends at its first empty line after the request line, with
//! empty lines before the request line skipped (§2.2); a body is as long
//! as its `Content-Length`, none without one, or chunked to its last
//! chunk and the empty line after it (§7.1). A head or a body hyper
//! refuses ends the connection, so no clock is asked of it. Where hyper
//! begins a request this reading has not seen end, the two have parted,
//! and the reading stops: nothing is ahead from then, the keep-alive clock
//! as before #2572.

/// How long a request's body is, as hyper frames it from its head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyLength {
    /// So many bytes: its `Content-Length`, or none without one.
    Exact(u64),
    /// Chunked, to its last chunk.
    Chunked,
}

/// The bytes a connection has read, against the requests they carry.
#[derive(Debug, Default)]
pub(crate) struct Boundary {
    at: At,
    /// What was read past a head whose request has not yet begun, held
    /// until its body's length is known.
    unframed: Vec<u8>,
}

/// Where the next byte read falls.
#[derive(Debug, Default)]
enum At {
    /// Before a request line. `blank_lines` whether an empty line came,
    /// which hyper holds as the start of the next request.
    #[default]
    Between,
    /// Empty lines came before a request line.
    BlankLines,
    /// In a head's lines; `empty` while its line holds nothing but CR.
    Head { empty: bool },
    /// Past a head, until its request begins and says its body's length.
    Unframed,
    /// In a body, so many bytes left.
    Sized(u64),
    /// In a chunked body.
    Chunked(Chunk),
    /// hyper began a request this reading had not seen the head of end.
    Parted,
}

/// Where a chunked body's next byte falls.
#[derive(Debug, Clone, Copy)]
enum Chunk {
    /// In a size line: its size so far, and whether its digits ended, at
    /// an extension or a space.
    Size { size: u64, digits: bool },
    /// In a chunk's data, so many bytes left.
    Data(u64),
    /// At the CRLF after a chunk's data.
    DataEnd,
    /// In the trailer section after the last chunk; `empty` while its
    /// line holds nothing but CR.
    Trailers { empty: bool },
}

impl Boundary {
    /// `bytes` read, in order after every byte read before them: whether
    /// a request after the ones whose bodies have ended has begun to
    /// arrive, which, once the reading has parted from hyper's, any byte
    /// is taken for.
    pub(crate) fn read(&mut self, bytes: &[u8]) -> bool {
        self.follow(bytes);
        self.ahead() || (matches!(self.at, At::Parted) && !bytes.is_empty())
    }

    /// `bytes` followed through the requests they carry.
    fn follow(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            bytes = match &mut self.at {
                At::Between | At::BlankLines => {
                    if let Some(start) = bytes.iter().position(|b| !matches!(b, b'\r' | b'\n')) {
                        self.at = At::Head { empty: false };
                        &bytes[start..]
                    } else {
                        self.at = At::BlankLines;
                        &[]
                    }
                }
                At::Head { empty } => match lines(bytes, empty) {
                    Some(rest) => {
                        self.at = At::Unframed;
                        rest
                    }
                    None => &[],
                },
                At::Unframed => {
                    self.unframed.extend_from_slice(bytes);
                    &[]
                }
                At::Sized(left) => {
                    let rest = taken(left, bytes);
                    if *left == 0 {
                        self.at = At::Between;
                    }
                    rest
                }
                At::Chunked(chunk) => {
                    let (rest, ended) = chunked(chunk, bytes);
                    if ended {
                        self.at = At::Between;
                    }
                    rest
                }
                At::Parted => &[],
            };
        }
    }

    /// The request whose head was read last has begun, its body `length`
    /// long: what was read past its head is read again as its body and
    /// what follows.
    pub(crate) fn began(&mut self, length: BodyLength) {
        if !matches!(self.at, At::Unframed) {
            self.at = At::Parted;
            self.unframed = Vec::new();
            return;
        }
        self.at = match length {
            BodyLength::Exact(0) => At::Between,
            BodyLength::Exact(n) => At::Sized(n),
            BodyLength::Chunked => At::Chunked(Chunk::Size {
                size: 0,
                digits: false,
            }),
        };
        let unframed = std::mem::take(&mut self.unframed);
        self.follow(&unframed);
        // The buffer kept for the next head, emptied.
        self.unframed = unframed;
        self.unframed.clear();
    }

    /// Whether any byte of a request after the ones whose bodies have
    /// ended has been read.
    pub(crate) fn ahead(&self) -> bool {
        matches!(self.at, At::BlankLines | At::Head { .. } | At::Unframed)
    }
}

/// `bytes` read as lines until an empty one, `empty` whether the line
/// they continue has held nothing but CR: what follows that empty line,
/// or `None` where it has not come.
fn lines<'a>(bytes: &'a [u8], empty: &mut bool) -> Option<&'a [u8]> {
    let mut at = 0;
    while let Some(lf) = bytes[at..].iter().position(|b| *b == b'\n') {
        let line = &bytes[at..at + lf];
        if *empty && line.iter().all(|b| *b == b'\r') {
            return Some(&bytes[at + lf + 1..]);
        }
        *empty = true;
        at += lf + 1;
    }
    if bytes[at..].iter().any(|b| *b != b'\r') {
        *empty = false;
    }
    None
}

/// What follows the `left` bytes of `bytes` a body has left, those taken
/// off `left`.
fn taken<'a>(left: &mut u64, bytes: &'a [u8]) -> &'a [u8] {
    let n = usize::try_from(*left).map_or(bytes.len(), |left| left.min(bytes.len()));
    *left -= u64::try_from(n).unwrap_or(*left);
    &bytes[n..]
}

/// `bytes` read into a chunked body at `chunk`: what follows, and whether
/// the body ended there.
fn chunked<'a>(chunk: &mut Chunk, mut bytes: &'a [u8]) -> (&'a [u8], bool) {
    while let Some((&byte, rest)) = bytes.split_first() {
        match chunk {
            Chunk::Size { size, digits } => {
                if byte == b'\n' {
                    *chunk = if *size == 0 {
                        Chunk::Trailers { empty: true }
                    } else {
                        Chunk::Data(*size)
                    };
                } else if let (false, Some(digit)) = (*digits, char::from(byte).to_digit(16)) {
                    *size = size.saturating_mul(16).saturating_add(u64::from(digit));
                } else if byte != b'\r' {
                    *digits = true;
                }
                bytes = rest;
            }
            Chunk::Data(left) => {
                bytes = taken(left, bytes);
                if *left == 0 {
                    *chunk = Chunk::DataEnd;
                }
            }
            Chunk::DataEnd => {
                if byte == b'\n' {
                    *chunk = Chunk::Size {
                        size: 0,
                        digits: false,
                    };
                }
                bytes = rest;
            }
            Chunk::Trailers { empty } => {
                return match lines(bytes, empty) {
                    Some(rest) => (rest, true),
                    None => (&[], false),
                };
            }
        }
    }
    (bytes, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parts` read one after another, then whether a next request's bytes
    /// were read, with the request begun after `began` parts.
    fn ahead(parts: &[&[u8]], began: usize, length: BodyLength) -> bool {
        let mut boundary = Boundary::default();
        for (n, part) in parts.iter().enumerate() {
            if n == began {
                boundary.began(length);
            }
            boundary.read(part);
        }
        if began >= parts.len() {
            boundary.began(length);
        }
        boundary.ahead()
    }

    #[test]
    fn a_head_read_with_the_start_of_the_next_is_ahead() {
        let both = b"GET / HTTP/1.1\r\nHost: a\r\n\r\nGET /next HT";
        assert!(ahead(&[both], 1, BodyLength::Exact(0)));
        assert!(!ahead(
            &[b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"],
            1,
            BodyLength::Exact(0)
        ));
        // The head's end split across reads, CR from LF.
        assert!(!ahead(
            &[b"GET / HTTP/1.1\r\nHost: a\r", b"\n\r", b"\n"],
            3,
            BodyLength::Exact(0)
        ));
        assert!(ahead(
            &[b"GET / HTTP/1.1\r\nHost: a\r", b"\n\r", b"\nG"],
            3,
            BodyLength::Exact(0)
        ));
        // Empty lines before a request line are skipped, and held ahead.
        assert!(!ahead(
            &[b"\r\n\r\nGET / HTTP/1.1\r\n\r\n"],
            1,
            BodyLength::Exact(0)
        ));
        assert!(ahead(
            &[b"GET / HTTP/1.1\r\n\r\n\r\n"],
            1,
            BodyLength::Exact(0)
        ));
    }

    #[test]
    fn a_sized_body_ends_at_its_length() {
        let request = b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        assert!(!ahead(&[request], 1, BodyLength::Exact(5)));
        assert!(!ahead(
            &[&request[..40], &request[40..]],
            1,
            BodyLength::Exact(5)
        ));
        let mut pipelined = request.to_vec();
        pipelined.extend_from_slice(b"GET");
        assert!(ahead(&[&pipelined], 1, BodyLength::Exact(5)));
        // Read after the request began, the body and the next head.
        assert!(ahead(
            &[&request[..38], b"hel", b"loG"],
            1,
            BodyLength::Exact(5)
        ));
        assert!(!ahead(
            &[&request[..38], b"hel", b"lo"],
            1,
            BodyLength::Exact(5)
        ));
    }

    #[test]
    fn a_chunked_body_ends_after_its_last_chunk() {
        let request = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n\
                        5;ext=1\r\nhello\r\nA\r\n0123456789\r\n0\r\n\r\n";
        assert!(!ahead(&[request], 1, BodyLength::Chunked));
        let mut pipelined = request.to_vec();
        pipelined.extend_from_slice(b"GET /");
        assert!(ahead(&[&pipelined], 1, BodyLength::Chunked));
        // Byte by byte, the request begun once its head was read.
        let head = request
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map_or(0, |end| end + 4);
        let bytes: Vec<&[u8]> = pipelined.chunks(1).collect();
        assert!(ahead(&bytes, head, BodyLength::Chunked));
        let bytes: Vec<&[u8]> = request.chunks(1).collect();
        assert!(!ahead(&bytes, head, BodyLength::Chunked));
    }

    #[test]
    fn a_request_begun_before_its_head_ended_parts_the_reading() {
        assert!(!ahead(
            &[b"GET / HTTP/1.1\r\n", b"\r\nGET"],
            1,
            BodyLength::Exact(0)
        ));
    }
}

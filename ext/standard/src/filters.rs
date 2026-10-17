//! `Z/ext/standard/filters.c`: the string filters, `string.rot13`,
//! `string.toupper` and `string.tolower`, each a [`StreamFilter`] that
//! maps every bucket it is handed byte for byte and passes it on; the
//! `convert.*` family ([`convert`]); `consumed`, which puts the stream
//! where the bytes it passed on end; and `dechunk`, HTTP's chunked
//! decoder. `PHP_MINIT(standard_filters)` registers the six in that
//! order, served here at request start since the request's filter table
//! is the one a program's `stream_filter_register` adds to.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::raising::Thrown;
use php_ext::request::{
    Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter, Streams as _, Whence,
};
use php_ext::values::{Pemalloc, Value};

mod convert;

/// One of the three string filters: the byte map it applies.
struct Strfilter {
    map: fn(u8) -> u8,
}

/// `rot13_from` to `rot13_to` (`Z/ext/standard/filters.c:28`).
const fn rot13(b: u8) -> u8 {
    match b {
        b'a'..=b'm' | b'A'..=b'M' => b + 13,
        b'n'..=b'z' | b'N'..=b'Z' => b - 13,
        _ => b,
    }
}

impl StreamFilter for Strfilter {
    /// `strfilter_rot13_filter` and its two siblings
    /// (`Z/ext/standard/filters.c:32`): every bucket mapped, counted as
    /// consumed and handed on, and `PSFS_PASS_ON`.
    fn filter(
        &mut self,
        _req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        _flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let mut count = 0;
        while let Some(mut bucket) = input.pop_front() {
            for b in &mut bucket {
                *b = (self.map)(*b);
            }
            count += bucket.len();
            output.push_back(bucket);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

/// The three factories, `strfilter_rot13_create` and its siblings.
#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn rot13_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
    _persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter { map: rot13 })))
}

#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn toupper_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
    _persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter {
        map: |b| b.to_ascii_uppercase(),
    })))
}

#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn tolower_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
    _persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(Strfilter {
        map: |b| b.to_ascii_lowercase(),
    })))
}

/// `sizeof(php_consumed_filter_data)` and `sizeof(php_chunked_filter_data)`
/// on the oracle's LP64.
const CONSUMED_DATA: usize = 24;
const CHUNKED_DATA: usize = 16;

/// `php_consumed_filter_data`: the stream's position at the filter's
/// first call, and the bytes it has passed on since; `data` is what php
/// `pecalloc`s for them, `None` once the `dtor` gave it back.
struct Consumed {
    offset: Option<i64>,
    passed: usize,
    data: Option<Pemalloc>,
}

impl StreamFilter for Consumed {
    /// `consumed_filter_dtor`: the data given back.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Some(data) = self.data.take() {
            data.free(req);
        }
        Ok(())
    }

    /// `consumed_filter_filter` (`Z/ext/standard/filters.c:1608`): every
    /// bucket passed on and counted, and under `PSFS_FLAG_FLUSH_CLOSE`
    /// the stream sought to the position it was at first with what
    /// earlier calls passed on added.
    fn filter(
        &mut self,
        req: &mut Request,
        stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let offset = *self.offset.get_or_insert_with(|| stream.tell(req));
        let mut count = 0;
        while let Some(bucket) = input.pop_front() {
            count += bucket.len();
            output.push_back(bucket);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        if flags == FilterFlags::FlushClose {
            let to = offset.saturating_add(i64::try_from(self.passed).unwrap_or(i64::MAX));
            stream.seek(req, to, Whence::Set);
        }
        self.passed += count;
        Ok(FilterStatus::PassOn)
    }
}

/// `consumed_filter_create` (`Z/ext/standard/filters.c:1654`).
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "abcdefghij"); rewind($m); fread($m, 2);
/// $f = stream_filter_append($m, 'consumed', STREAM_FILTER_READ);
/// var_dump(fread($m, 3), ftell($m));
/// var_dump(stream_filter_remove($f), ftell($m), fread($m, 100));
/// $w = fopen('php://memory', 'w+');
/// $f = stream_filter_append($w, 'consumed', STREAM_FILTER_WRITE);
/// fwrite($w, "hello"); fwrite($w, "world"); stream_filter_remove($f);
/// var_dump(ftell($w));
/// ```
/// ```expect
/// string(3) "cde"
/// int(5)
/// bool(true)
/// int(10)
/// string(0) ""
/// int(10)
/// ```
fn consumed_factory(
    req: &mut Request,
    name: &[u8],
    _params: Option<&Value>,
    persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    if !name.eq_ignore_ascii_case(b"consumed") {
        return Ok(None);
    }
    Ok(Some(Box::new(Consumed {
        offset: None,
        passed: 0,
        data: Some(Pemalloc::new(req, CONSUMED_DATA, persistent)?),
    })))
}

/// `php_chunked_filter_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    SizeStart,
    Size,
    SizeExt,
    SizeCr,
    SizeLf,
    Body,
    BodyCr,
    BodyLf,
    Trailer,
    Error,
}

/// `php_chunked_filter_data`: where the decoder stands, and what is left
/// of the chunk it is in; `data` is what php `pecalloc`s for them, `None`
/// once the `dtor` gave it back.
struct Dechunk {
    chunk_size: usize,
    state: ChunkState,
    data: Option<Pemalloc>,
}

impl Dechunk {
    /// `php_dechunk` (`Z/ext/standard/filters.c:1699`): the chunks' bodies
    /// moved to the front of `buf`, their sizes, extensions, line ends
    /// and the trailer dropped; bytes that are no chunk pass as they
    /// are, and so does everything after them. The length of what is
    /// left.
    #[allow(clippy::too_many_lines)] // one function in php, ported as one
    fn dechunk(&mut self, buf: &mut [u8]) -> usize {
        let end = buf.len();
        let mut p = 0;
        let mut out = 0;
        while p < end {
            let mut state = self.state;
            if state == ChunkState::SizeStart {
                self.chunk_size = 0;
            }
            if matches!(state, ChunkState::SizeStart | ChunkState::Size) {
                while p < end {
                    let digit = match buf[p] {
                        c @ b'0'..=b'9' => c - b'0',
                        c @ b'A'..=b'F' => c - b'A' + 10,
                        c @ b'a'..=b'f' => c - b'a' + 10,
                        _ if self.state == ChunkState::SizeStart => {
                            self.state = ChunkState::Error;
                            break;
                        }
                        _ => {
                            self.state = ChunkState::SizeExt;
                            break;
                        }
                    };
                    self.chunk_size = self
                        .chunk_size
                        .wrapping_mul(16)
                        .wrapping_add(usize::from(digit));
                    self.state = ChunkState::Size;
                    p += 1;
                }
                if self.state == ChunkState::Error {
                    continue;
                } else if p == end {
                    return out;
                }
                state = ChunkState::SizeExt;
            }
            if state == ChunkState::SizeExt {
                while p < end && buf[p] != b'\r' && buf[p] != b'\n' {
                    p += 1;
                }
                if p == end {
                    return out;
                }
                state = ChunkState::SizeCr;
            }
            if state == ChunkState::SizeCr {
                if buf[p] == b'\r' {
                    p += 1;
                    if p == end {
                        self.state = ChunkState::SizeLf;
                        return out;
                    }
                }
                state = ChunkState::SizeLf;
            }
            if state == ChunkState::SizeLf {
                if buf[p] == b'\n' {
                    p += 1;
                    if self.chunk_size == 0 {
                        self.state = ChunkState::Trailer;
                        continue;
                    } else if p == end {
                        self.state = ChunkState::Body;
                        return out;
                    }
                } else {
                    self.state = ChunkState::Error;
                    continue;
                }
                state = ChunkState::Body;
            }
            if state == ChunkState::Body {
                if end - p >= self.chunk_size {
                    buf.copy_within(p..p + self.chunk_size, out);
                    out += self.chunk_size;
                    p += self.chunk_size;
                    if p == end {
                        self.state = ChunkState::BodyCr;
                        return out;
                    }
                } else {
                    buf.copy_within(p..end, out);
                    self.chunk_size -= end - p;
                    self.state = ChunkState::Body;
                    out += end - p;
                    return out;
                }
                state = ChunkState::BodyCr;
            }
            if state == ChunkState::BodyCr {
                if buf[p] == b'\r' {
                    p += 1;
                    if p == end {
                        self.state = ChunkState::BodyLf;
                        return out;
                    }
                }
                state = ChunkState::BodyLf;
            }
            match state {
                ChunkState::BodyLf => {
                    if buf[p] == b'\n' {
                        p += 1;
                        self.state = ChunkState::SizeStart;
                    } else {
                        self.state = ChunkState::Error;
                    }
                }
                ChunkState::Trailer => p = end,
                ChunkState::Error => {
                    buf.copy_within(p..end, out);
                    out += end - p;
                    return out;
                }
                _ => {}
            }
        }
        out
    }
}

impl StreamFilter for Dechunk {
    /// `php_chunked_dtor`: the data given back.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Some(data) = self.data.take() {
            data.free(req);
        }
        Ok(())
    }

    /// `php_chunked_filter` (`Z/ext/standard/filters.c:1827`): every
    /// bucket decoded in place, counted whole, and passed on.
    fn filter(
        &mut self,
        _req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        _flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let mut count = 0;
        while let Some(mut bucket) = input.pop_front() {
            count += bucket.len();
            let len = self.dechunk(&mut bucket);
            bucket.truncate(len);
            output.push_back(bucket);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

/// `chunked_filter_create` (`Z/ext/standard/filters.c:1868`).
///
/// ```php
/// $m = fopen('php://memory', 'w+');
/// fwrite($m, "5\r\nHello\r\n7;ext=1\r\n, world\r\n0\r\nTrailer: x\r\n\r\n"); rewind($m);
/// stream_filter_append($m, 'dechunk', STREAM_FILTER_READ);
/// var_dump(stream_get_contents($m));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "not chunked"); rewind($m);
/// stream_filter_append($m, 'dechunk', STREAM_FILTER_READ);
/// var_dump(stream_get_contents($m));
/// ```
/// ```expect
/// string(12) "Hello, world"
/// string(11) "not chunked"
/// ```
fn dechunk_factory(
    req: &mut Request,
    name: &[u8],
    _params: Option<&Value>,
    persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    if !name.eq_ignore_ascii_case(b"dechunk") {
        return Ok(None);
    }
    Ok(Some(Box::new(Dechunk {
        chunk_size: 0,
        state: ChunkState::SizeStart,
        data: Some(Pemalloc::new(req, CHUNKED_DATA, persistent)?),
    })))
}

/// `PHP_MINIT(standard_filters)` (`Z/ext/standard/filters.c:1907`)'s
/// six filters registered in its order, at request start, the
/// `convert.*` family under its wildcard.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "Hello, World!\n"); rewind($m);
/// stream_filter_append($m, 'string.rot13');
/// var_dump(fgets($m));
/// foreach (['string.toupper', 'string.tolower'] as $f) {
///     $w = fopen('php://memory', 'w+'); stream_filter_append($w, $f, STREAM_FILTER_WRITE);
///     fwrite($w, "MiXeD 123"); rewind($w); var_dump(fread($w, 20));
/// }
/// $all = stream_get_filters();
/// echo implode(' ', array_slice($all, array_search('string.rot13', $all), 6)), "\n";
/// ```
/// ```expect
/// string(14) "Uryyb, Jbeyq!
/// "
/// string(9) "MIXED 123"
/// string(9) "mixed 123"
/// string.rot13 string.toupper string.tolower convert.* consumed dechunk
/// ```
pub(crate) fn register(req: &mut Request) {
    req.register_filter(b"string.rot13", rot13_factory);
    req.register_filter(b"string.toupper", toupper_factory);
    req.register_filter(b"string.tolower", tolower_factory);
    req.register_filter(b"convert.*", convert::create);
    req.register_filter(b"consumed", consumed_factory);
    req.register_filter(b"dechunk", dechunk_factory);
}

//! `zlib_filter.c`: the `zlib.*` factory and its two filters,
//! `zlib.inflate` and `zlib.deflate`, each feeding zlib from its buckets
//! in pieces of the 32 KiB `inbuf` php copies through and handing on
//! what fits the 32 KiB `outbuf` after every call, as php's do.

use php_ext::coerce::to_long;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::walk;
use php_ext::values::{Key, Pemalloc, Value, ValueRef};

use crate::encode::MAX_MEM_LEVEL;
use crate::ffi::{Deflate, Flush, Inflate, Status};

/// The name `MINIT` registers the factory under.
pub(crate) const NAME: &[u8] = b"zlib.*";

/// The size of php's `inbuf` and `outbuf`, `0x8000`.
const BUFFER: usize = 0x8000;

/// `sizeof(php_zlib_filter_data)` on the oracle's LP64: the 112-byte
/// `z_stream`, the two buffers and their lengths, `persistent` and
/// `finished`.
const DATA: usize = 152;

/// `MAX_WBITS`.
const MAX_WBITS: i64 = 15;

/// What `php_zlib_filter_create` allocates beside zlib's state, with the
/// stream's persistence: `data`, the `php_zlib_filter_data`, and its
/// `inbuf`, which each piece of a bucket is copied through, and `outbuf`,
/// which each call writes into. zlib's own state is on the request's heap
/// whatever the stream, as php's `php_zlib_alloc` reads a `persistent`
/// the factory never sets.
struct Buffers {
    data: Pemalloc,
    inbuf: Pemalloc,
    outbuf: Pemalloc,
}

impl Buffers {
    /// `pecalloc(1, sizeof(php_zlib_filter_data), persistent)` and the
    /// two `pemalloc(0x8000, persistent)`s; the `Err` is the memory
    /// limit's fatal.
    fn new(req: &mut Request, persistent: bool) -> Result<Buffers, Thrown> {
        Ok(Buffers {
            data: Pemalloc::new(req, DATA, persistent)?,
            inbuf: Pemalloc::new(req, BUFFER, persistent)?,
            outbuf: Pemalloc::new(req, BUFFER, persistent)?,
        })
    }

    /// `piece` copied into `inbuf`, as php's `memcpy` before each call:
    /// the input, and `outbuf` to write into.
    fn take_in(&mut self, piece: &[u8]) -> (&[u8], &mut [u8]) {
        let inbuf = &mut self.inbuf.bytes_mut()[..piece.len()];
        inbuf.copy_from_slice(piece);
        (inbuf, self.outbuf.bytes_mut())
    }

    /// The first `written` bytes of `outbuf`, `estrndup(data->outbuf,
    /// bucketlen)` for the bucket handed on.
    fn out(&self, written: usize) -> Vec<u8> {
        self.outbuf.bytes()[..written].to_vec()
    }

    /// The dtor's `pefree`s, in php's order.
    fn free(self, req: &mut Request) {
        self.inbuf.free(req);
        self.outbuf.free(req);
        self.data.free(req);
    }
}

/// `zlib.inflate`: the stream, ended and dropped once it reached its end,
/// and the buffers, `None` once the dtor freed them.
struct Inflating {
    stream: Option<Inflate>,
    buffers: Option<Buffers>,
}

/// `zlib.deflate`: the stream, `None` once the filter's `dtor` ended it,
/// the buffers, `None` once the dtor freed them, and `finished`, whether
/// no flush is pending since the last bucket.
struct Deflating {
    stream: Option<Deflate>,
    buffers: Option<Buffers>,
    finished: bool,
}

impl StreamFilter for Inflating {
    /// `php_zlib_inflate_filter`.
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let flush = if flags == FilterFlags::FlushClose {
            Flush::Finish
        } else {
            Flush::Sync
        };
        let Some(buffers) = self.buffers.as_mut() else {
            return Ok(FilterStatus::ErrFatal);
        };
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                let Some(stream) = self.stream.as_mut() else {
                    break;
                };
                let desired = (bucket.len() - bin).min(BUFFER);
                let (piece, out) = buffers.take_in(&bucket[bin..bin + desired]);
                let step = stream.inflate(req, piece, out, flush)?;
                match step.status {
                    Status::StreamEnd => {
                        if let Some(stream) = self.stream.take() {
                            stream.end(req);
                        }
                        exit = FilterStatus::PassOn;
                    }
                    Status::Ok | Status::BufError => {}
                    failed => {
                        let text = format!("zlib: {}", failed.message());
                        docref(req, Level::Notice, &text);
                        return Ok(FilterStatus::ErrFatal);
                    }
                }
                bin += step.read;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
            }
            total += bucket.len();
        }
        if flags == FilterFlags::FlushClose
            && let Some(stream) = self.stream.as_mut()
        {
            loop {
                let step = stream.inflate(req, &[], buffers.outbuf.bytes_mut(), Flush::Finish)?;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
                if step.status != Status::Ok {
                    break;
                }
            }
        }
        if let Some(consumed) = consumed {
            *consumed = total;
        }
        Ok(exit)
    }

    /// `php_zlib_inflate_dtor`: `inflateEnd` where the stream has not
    /// reached its end, the state given back, then the buffers and
    /// `data` freed.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Some(stream) = self.stream.take() {
            stream.end(req);
        }
        if let Some(buffers) = self.buffers.take() {
            buffers.free(req);
        }
        Ok(())
    }
}

impl StreamFilter for Deflating {
    /// `php_zlib_deflate_filter`.
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let (Some(stream), Some(buffers)) = (self.stream.as_mut(), self.buffers.as_mut()) else {
            return Ok(FilterStatus::ErrFatal);
        };
        let mode = match flags {
            FilterFlags::FlushClose => Flush::Full,
            FilterFlags::FlushInc => Flush::Sync,
            FilterFlags::Normal => Flush::None,
        };
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                let desired = (bucket.len() - bin).min(BUFFER);
                self.finished = mode != Flush::None;
                let (piece, out) = buffers.take_in(&bucket[bin..bin + desired]);
                let step = stream.deflate(req, piece, out, mode)?;
                if step.status != Status::Ok {
                    return Ok(FilterStatus::ErrFatal);
                }
                bin += step.read;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
            }
            total += bucket.len();
        }
        let close = flags == FilterFlags::FlushClose;
        if close || (flags == FilterFlags::FlushInc && !self.finished) {
            let last = if close { Flush::Finish } else { Flush::Sync };
            loop {
                let step = stream.deflate(req, &[], buffers.outbuf.bytes_mut(), last)?;
                self.finished = true;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
                if step.status != Status::Ok {
                    break;
                }
            }
        }
        if let Some(consumed) = consumed {
            *consumed = total;
        }
        Ok(exit)
    }

    /// `php_zlib_deflate_dtor`: `deflateEnd`, the state given back, then
    /// the buffers and `data` freed.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Some(stream) = self.stream.take() {
            stream.end(req);
        }
        if let Some(buffers) = self.buffers.take() {
            buffers.free(req);
        }
        Ok(())
    }
}

/// A parameter of an array or object `filterparams`,
/// `zend_hash_str_find_ind(HASH_OF(filterparams), name)`.
fn param(req: &mut Request, params: &Value, name: &[u8]) -> Result<Option<Value>, Thrown> {
    match params.as_ref() {
        ValueRef::Array(array) => Ok(array.find(name).map(|value| value.deref())),
        ValueRef::Object(object) => {
            let table = object.property_table(req)?;
            Ok(walk::rows(req, table)
                .find(|(key, _)| matches!(key, Key::Str(k) if k.as_bytes() == name))
                .map(|(_, value)| value.deref()))
        }
        _ => Ok(None),
    }
}

/// `php_zlib_filter_create`: `zlib.inflate` with its `window`, from raw
/// deflate's -15 to 47, automatic header detection; `zlib.deflate` with
/// `memory`, `window` and `level` from an array or object, or the level
/// alone as a scalar. A parameter out of range is php's warning and the
/// default kept; a name that is neither filter makes none.
///
/// ```php
/// $data = str_repeat("The quick brown fox jumps over the lazy dog.\n", 100);
/// $fp = fopen('php://memory', 'w+');
/// $f = stream_filter_append($fp, 'zlib.deflate', STREAM_FILTER_WRITE, ['level' => 9, 'window' => 15, 'memory' => 9]);
/// fwrite($fp, $data);
/// stream_filter_remove($f);
/// rewind($fp);
/// $z = stream_get_contents($fp);
/// echo strlen($z), ' ', md5($z), "\n";
/// var_dump(gzinflate($z) === $data);
/// rewind($fp);
/// stream_filter_append($fp, 'zlib.inflate', STREAM_FILTER_READ);
/// var_dump(stream_get_contents($fp) === $data);
/// $fp = fopen('php://memory', 'w+');
/// stream_filter_append($fp, 'zlib.deflate', STREAM_FILTER_WRITE, 11);
/// stream_filter_append($fp, 'zlib.deflate', STREAM_FILTER_WRITE, ['window' => 99, 'memory' => 0]);
/// stream_filter_append($fp, 'zlib.deflate', STREAM_FILTER_WRITE, true);
/// var_dump(@stream_filter_append($fp, 'zlib.nope'));
/// fclose($fp);
/// $fp = fopen('php://memory', 'w+');
/// fwrite($fp, gzencode('gz data'));
/// rewind($fp);
/// stream_filter_append($fp, 'zlib.inflate', STREAM_FILTER_READ, ['window' => 31]);
/// var_dump(stream_get_contents($fp));
/// ```
/// ```expect
/// 81 8f7b748b01c2e04f2a53ea1df54f2a4b
///
/// Warning: gzinflate(): data error in %s on line 10
/// bool(false)
///
/// Notice: stream_get_contents(): zlib: data error in %s on line 13
/// bool(false)
///
/// Warning: stream_filter_append(): Invalid compression level specified. (11) in %s on line 15
///
/// Warning: stream_filter_append(): Invalid parameter given for memory level (0) in %s on line 16
///
/// Warning: stream_filter_append(): Invalid parameter given for window size (99) in %s on line 16
///
/// Warning: stream_filter_append(): Invalid filter parameter, ignored in %s on line 17
/// bool(false)
/// string(7) "gz data"
/// ```
pub(crate) fn factory(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
    persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    let buffers = Buffers::new(req, persistent)?;
    if name.eq_ignore_ascii_case(b"zlib.inflate") {
        let mut window = -MAX_WBITS;
        if let Some(params) = params
            && matches!(params.as_ref(), ValueRef::Array(_) | ValueRef::Object(_))
            && let Some(value) = param(req, params, b"window")?
        {
            let tmp = to_long(req, &value)?;
            if (-MAX_WBITS..=MAX_WBITS + 32).contains(&tmp) {
                window = tmp;
            } else {
                let text = format!("Invalid parameter given for window size ({tmp})");
                docref(req, Level::Warning, &text);
            }
        }
        let Ok(stream) = Inflate::new(req, window)? else {
            buffers.free(req);
            return Ok(None);
        };
        return Ok(Some(Box::new(Inflating {
            stream: Some(stream),
            buffers: Some(buffers),
        })));
    }
    if !name.eq_ignore_ascii_case(b"zlib.deflate") {
        buffers.free(req);
        return Ok(None);
    }
    let (mut level, mut window, mut memory) = (-1, -MAX_WBITS, MAX_MEM_LEVEL);
    if let Some(params) = params {
        let asked = match params.as_ref() {
            ValueRef::Array(_) | ValueRef::Object(_) => {
                if let Some(value) = param(req, params, b"memory")? {
                    let tmp = to_long(req, &value)?;
                    if (1..=MAX_MEM_LEVEL).contains(&tmp) {
                        memory = tmp;
                    } else {
                        let text = format!("Invalid parameter given for memory level ({tmp})");
                        docref(req, Level::Warning, &text);
                    }
                }
                if let Some(value) = param(req, params, b"window")? {
                    let tmp = to_long(req, &value)?;
                    if (-MAX_WBITS..=MAX_WBITS + 16).contains(&tmp) {
                        window = tmp;
                    } else {
                        let text = format!("Invalid parameter given for window size ({tmp})");
                        docref(req, Level::Warning, &text);
                    }
                }
                match param(req, params, b"level")? {
                    Some(value) => Some(to_long(req, &value)?),
                    None => None,
                }
            }
            ValueRef::Str(_) | ValueRef::Float(_) | ValueRef::Int(_) => Some(to_long(req, params)?),
            _ => {
                docref(req, Level::Warning, "Invalid filter parameter, ignored");
                None
            }
        };
        if let Some(tmp) = asked {
            if (-1..=9).contains(&tmp) {
                level = tmp;
            } else {
                let text = format!("Invalid compression level specified. ({tmp})");
                docref(req, Level::Warning, &text);
            }
        }
    }
    let Ok(stream) = Deflate::new(req, level, window, memory, 0)? else {
        buffers.free(req);
        return Ok(None);
    };
    Ok(Some(Box::new(Deflating {
        stream: Some(stream),
        buffers: Some(buffers),
        finished: true,
    })))
}

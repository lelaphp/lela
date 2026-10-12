//! `bz2_filter.c`: the `bzip2.*` factory and its two filters,
//! `bzip2.decompress` and `bzip2.compress`, each feeding the library
//! from its buckets in pieces of the 2048-byte `inbuf` php copies
//! through and handing on what fits the 2048-byte `outbuf` after every
//! call, as php's do.

use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::walk;
use php_ext::values::{Emalloc, Key, Value, ValueRef};

use crate::ffi::{Action, Code, LentCompress, LentDecompress};

/// The name `MINIT` registers the factory under.
pub(crate) const NAME: &[u8] = b"bzip2.*";

/// The size of php's `inbuf` and `outbuf`.
const BUFFER: usize = 2048;

/// `sizeof(php_bz2_filter_data)` on the oracle's LP64: the 80-byte
/// `bz_stream`, the two buffers and their lengths, the status, the bits
/// and `persistent`.
const DATA: usize = 128;

/// `PHP_BZ2_FILTER_DEFAULT_BLOCKSIZE` and `_WORKFACTOR`.
const DEFAULT_BLOCKS: i64 = 4;
const DEFAULT_WORK: i64 = 0;

/// What `php_bz2_filter_create` allocates beside the library's state, on
/// the request's heap as a filter that is not persistent has them
/// (design §5.13): `data`, the `php_bz2_filter_data`, and its `inbuf`,
/// which each piece of a bucket is copied through, and `outbuf`, which
/// each call writes into.
struct Buffers {
    data: Emalloc,
    inbuf: Emalloc,
    outbuf: Emalloc,
}

impl Buffers {
    /// `pecalloc(1, sizeof(php_bz2_filter_data))` and the two
    /// `pemalloc(2048)`s; the `Err` is the memory limit's fatal.
    fn new(req: &mut Request) -> Result<Buffers, Thrown> {
        Ok(Buffers {
            data: Emalloc::new(req, DATA)?,
            inbuf: Emalloc::new(req, BUFFER)?,
            outbuf: Emalloc::new(req, BUFFER)?,
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

/// `enum strm_status` with the stream it runs: none made yet, one
/// running, or one ended and not to be made again.
enum Status {
    Uninitialized,
    Running(LentDecompress),
    Finished,
}

/// `bzip2.decompress`: the status, `small_footprint` and
/// `expect_concatenated`, and the buffers, `None` once the dtor freed them.
struct Decompressing {
    status: Status,
    small: bool,
    concatenated: bool,
    buffers: Option<Buffers>,
}

/// `bzip2.compress`: the stream and the buffers, `None` once the dtor
/// ended and freed them, and `is_flushed`, whether no flush is pending
/// since the last bucket.
struct Compressing {
    stream: Option<(LentCompress, Buffers)>,
    flushed: bool,
}

impl StreamFilter for Decompressing {
    /// `php_bz2_decompress_filter`.
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let Some(buffers) = self.buffers.as_mut() else {
            return Ok(FilterStatus::ErrFatal);
        };
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                if matches!(self.status, Status::Uninitialized) {
                    let Ok(stream) = LentDecompress::new(req, self.small)? else {
                        return Ok(FilterStatus::ErrFatal);
                    };
                    self.status = Status::Running(stream);
                }
                let Status::Running(stream) = &mut self.status else {
                    total += bucket.len();
                    break;
                };
                let desired = (bucket.len() - bin).min(BUFFER);
                let (piece, out) = buffers.take_in(&bucket[bin..bin + desired]);
                let step = stream.decompress(req, piece, out)?;
                match step.code {
                    Code::StreamEnd => {
                        let next = if self.concatenated {
                            Status::Uninitialized
                        } else {
                            Status::Finished
                        };
                        if let Status::Running(ended) = std::mem::replace(&mut self.status, next) {
                            ended.end(req);
                        }
                    }
                    Code::Ok => {}
                    _ => {
                        docref(req, Level::Notice, "bzip2 decompression failed");
                        return Ok(FilterStatus::ErrFatal);
                    }
                }
                total += step.read;
                bin += step.read;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                } else if step.code == Code::StreamEnd {
                    return Ok(FilterStatus::PassOn);
                }
            }
        }
        if flags == FilterFlags::FlushClose
            && let Status::Running(stream) = &mut self.status
        {
            loop {
                let step = stream.decompress(req, &[], buffers.outbuf.bytes_mut())?;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                } else if step.code == Code::Ok {
                    break;
                }
                if step.code != Code::Ok {
                    break;
                }
            }
        }
        if let Some(consumed) = consumed {
            *consumed = total;
        }
        Ok(exit)
    }

    /// `php_bz2_decompress_dtor`: `BZ2_bzDecompressEnd` where the stream
    /// is running, then the buffers and `data` freed.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Status::Running(stream) = std::mem::replace(&mut self.status, Status::Finished) {
            stream.end(req);
        }
        if let Some(buffers) = self.buffers.take() {
            buffers.free(req);
        }
        Ok(())
    }
}

impl StreamFilter for Compressing {
    /// `php_bz2_compress_filter`.
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let Some((stream, buffers)) = self.stream.as_mut() else {
            return Ok(FilterStatus::ErrFatal);
        };
        let action = match flags {
            FilterFlags::FlushClose => Action::Finish,
            FilterFlags::FlushInc => Action::Flush,
            FilterFlags::Normal => Action::Run,
        };
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                let desired = (bucket.len() - bin).min(BUFFER);
                self.flushed = action != Action::Run;
                let (piece, out) = buffers.take_in(&bucket[bin..bin + desired]);
                let step = stream.compress(req, piece, out, action)?;
                if !matches!(step.code, Code::RunOk | Code::FlushOk | Code::FinishOk) {
                    return Ok(FilterStatus::ErrFatal);
                }
                total += step.read;
                bin += step.read;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
            }
        }
        let close = flags == FilterFlags::FlushClose;
        if close || (flags == FilterFlags::FlushInc && !self.flushed) {
            let (last, going) = if close {
                (Action::Finish, Code::FinishOk)
            } else {
                (Action::Flush, Code::FlushOk)
            };
            loop {
                let step = stream.compress(req, &[], buffers.outbuf.bytes_mut(), last)?;
                self.flushed = true;
                if step.written > 0 {
                    output.push_back(buffers.out(step.written));
                    exit = FilterStatus::PassOn;
                }
                if step.code != going {
                    break;
                }
            }
        }
        if let Some(consumed) = consumed {
            *consumed = total;
        }
        Ok(exit)
    }

    /// `php_bz2_compress_dtor`: `BZ2_bzCompressEnd`, then the buffers and
    /// `data` freed.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if let Some((stream, buffers)) = self.stream.take() {
            stream.end(req);
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

/// `php_bz2_filter_create`: `bzip2.decompress` with `concatenated` and
/// `small` from an array or object, or `small` alone as any other value;
/// `bzip2.compress` with `blocks` and `work` from an array or object, a
/// value out of range php's warning and the default kept; a name that is
/// neither filter makes none.
///
/// ```php
/// $data = str_repeat("I am the very model of a modern major general.\n", 300);
/// foreach ([null, ['blocks' => 1], ['blocks' => 9, 'work' => 250], (object) ['blocks' => 2]] as $p) {
///     $fp = fopen('php://memory', 'w+');
///     $f = stream_filter_append($fp, 'bzip2.compress', STREAM_FILTER_WRITE, $p);
///     fwrite($fp, $data);
///     fflush($fp);
///     $flushed = strlen(stream_get_contents($fp, -1, 0));
///     stream_filter_remove($f);
///     $z = stream_get_contents($fp, -1, 0);
///     echo $flushed, ' ', strlen($z), ' ', md5($z), "\n";
/// }
/// $fp = fopen('php://memory', 'w+');
/// $f = stream_filter_append($fp, 'bzip2.compress', STREAM_FILTER_WRITE, ['blocks' => 9]);
/// fwrite($fp, $data);
/// stream_filter_remove($f);
/// $z = stream_get_contents($fp, -1, 0);
/// var_dump($z === bzcompress($data, 9), bzdecompress($z) === $data);
/// rewind($fp);
/// stream_filter_append($fp, 'bzip2.decompress', STREAM_FILTER_READ);
/// var_dump(stream_get_contents($fp) === $data);
/// $fp = fopen('php://memory', 'w+');
/// fwrite($fp, bzcompress('one') . bzcompress('two'));
/// rewind($fp);
/// stream_filter_append($fp, 'bzip2.decompress', STREAM_FILTER_READ, ['concatenated' => true]);
/// var_dump(stream_get_contents($fp));
/// rewind($fp);
/// stream_filter_append($fp, 'bzip2.decompress', STREAM_FILTER_READ, true);
/// var_dump(stream_get_contents($fp));
/// $fp = fopen('php://memory', 'w+');
/// stream_filter_append($fp, 'bzip2.compress', STREAM_FILTER_WRITE, ['blocks' => 10, 'work' => -1]);
/// var_dump(@stream_filter_append($fp, 'bzip2.nope'));
/// fwrite($fp, 'not bzip2');
/// rewind($fp);
/// stream_filter_append($fp, 'bzip2.decompress', STREAM_FILTER_READ);
/// var_dump(fread($fp, 10));
/// ```
/// ```expect
/// 134 146 59c810447d8da14f0abddbeee66ca2b3
/// 134 146 389e4e8bd35ecdb374daaf379e909b4b
/// 134 146 761550cf84424ce16c7c50fc3b2ee980
/// 134 146 0f9c8c6025705d4f82d4a1f4cd16c55d
/// bool(true)
/// bool(true)
/// bool(true)
/// string(6) "onetwo"
///
/// Notice: stream_get_contents(): bzip2 decompression failed in %s on line 29
/// string(0) ""
///
/// Warning: stream_filter_append(): Invalid parameter given for number of blocks to allocate (10) in %s on line 31
///
/// Warning: stream_filter_append(): Invalid parameter given for work factor (-1) in %s on line 31
/// bool(false)
/// string(0) ""
/// ```
///
/// A filter that is not persistent keeps `data`, its two buffers and the
/// library's state on the request's heap, as `php_bz2_alloc` points the
/// library at it. Each filter is counted past what `string.rot13`, which
/// allocates nothing of its own, counts; the decompressor's state is made
/// as the first bucket comes, and given back at its stream's end or with
/// the filter.
///
/// ```php
/// $f = fopen('php://memory', 'w+');
/// $w = fopen('php://memory', 'w+'); stream_filter_append($w, 'bzip2.compress', STREAM_FILTER_WRITE);
/// $m = memory_get_usage(); $r = stream_filter_append($f, 'string.rot13', STREAM_FILTER_WRITE); $base = memory_get_usage() - $m;
/// $m = memory_get_usage(); $c = stream_filter_append($f, 'bzip2.compress', STREAM_FILTER_WRITE); echo memory_get_usage() - $m - $base, "\n";
/// $m = memory_get_usage(); $c9 = stream_filter_append($f, 'bzip2.compress', STREAM_FILTER_WRITE, ['blocks' => 9]); echo memory_get_usage() - $m - $base, "\n";
/// $m = memory_get_usage(); $d = stream_filter_append($f, 'bzip2.decompress', STREAM_FILTER_READ); echo memory_get_usage() - $m - $base, "\n";
/// $m = memory_get_usage(); stream_filter_remove($r); $base = memory_get_usage() - $m;
/// $m = memory_get_usage(); stream_filter_remove($c9); echo memory_get_usage() - $m - $base, "\n";
/// $s = ''; for ($i = 0; $i < 20000; $i++) $s .= md5($i);
/// foreach ([false, true] as $small) {
///     $g = fopen('php://memory', 'w+'); fwrite($g, bzcompress($s)); rewind($g);
///     $d = stream_filter_append($g, 'bzip2.decompress', STREAM_FILTER_READ, $small);
///     $m = memory_get_usage(); $t = fread($g, 100); echo strlen($t), ' ', memory_get_usage() - $m, "\n";
///     $m = memory_get_usage(); stream_filter_remove($d); echo memory_get_usage() - $m - $base, "\n";
/// }
/// ```
/// ```expect
/// 3530720
/// 7536656
/// 4064
/// -7536776
/// 100 2068608
/// -1671296
/// 100 1470464
/// -1073280
/// ```
pub(crate) fn factory(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    let buffers = Buffers::new(req)?;
    if name.eq_ignore_ascii_case(b"bzip2.decompress") {
        let (mut small, mut concatenated) = (false, false);
        if let Some(params) = params {
            let asked = if matches!(params.as_ref(), ValueRef::Array(_) | ValueRef::Object(_)) {
                if let Some(value) = param(req, params, b"concatenated")? {
                    concatenated = truthy(req, &value)?;
                }
                param(req, params, b"small")?
            } else {
                Some(*params)
            };
            if let Some(value) = asked {
                small = truthy(req, &value)?;
            }
        }
        return Ok(Some(Box::new(Decompressing {
            status: Status::Uninitialized,
            small,
            concatenated,
            buffers: Some(buffers),
        })));
    }
    if !name.eq_ignore_ascii_case(b"bzip2.compress") {
        buffers.free(req);
        return Ok(None);
    }
    let (mut blocks, mut work) = (DEFAULT_BLOCKS, DEFAULT_WORK);
    if let Some(params) = params
        && matches!(params.as_ref(), ValueRef::Array(_) | ValueRef::Object(_))
    {
        if let Some(value) = param(req, params, b"blocks")? {
            let asked = to_long(req, &value)?;
            if (1..=9).contains(&asked) {
                blocks = asked;
            } else {
                let text =
                    format!("Invalid parameter given for number of blocks to allocate ({asked})");
                docref(req, Level::Warning, &text);
            }
        }
        if let Some(value) = param(req, params, b"work")? {
            let asked = to_long(req, &value)?;
            if (0..=250).contains(&asked) {
                work = asked;
            } else {
                let text = format!("Invalid parameter given for work factor ({asked})");
                docref(req, Level::Warning, &text);
            }
        }
    }
    // Both are in range, so each is an `int`.
    let (blocks, work) = (
        i32::try_from(blocks).unwrap_or(9),
        i32::try_from(work).unwrap_or(0),
    );
    let Ok(stream) = LentCompress::new(req, blocks, work)? else {
        buffers.free(req);
        return Ok(None);
    };
    Ok(Some(Box::new(Compressing {
        stream: Some((stream, buffers)),
        flushed: true,
    })))
}

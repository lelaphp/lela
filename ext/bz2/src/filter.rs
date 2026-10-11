//! `bz2_filter.c`: the `bzip2.*` factory and its two filters,
//! `bzip2.decompress` and `bzip2.compress`, each feeding the library
//! from its buckets in pieces of the 2048-byte `inbuf` php copies
//! through and handing on what fits the 2048-byte `outbuf` after every
//! call, as php's do.

use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::walk;
use php_ext::values::{Key, Value, ValueRef};

use crate::ffi::{Action, Code, Compress, Decompress};

/// The name `MINIT` registers the factory under.
pub(crate) const NAME: &[u8] = b"bzip2.*";

/// The size of php's `inbuf` and `outbuf`.
const BUFFER: usize = 2048;

/// `PHP_BZ2_FILTER_DEFAULT_BLOCKSIZE` and `_WORKFACTOR`.
const DEFAULT_BLOCKS: i64 = 4;
const DEFAULT_WORK: i64 = 0;

/// `enum strm_status` with the stream it runs: none made yet, one
/// running, or one ended and not to be made again.
enum Status {
    Uninitialized,
    Running(Decompress),
    Finished,
}

/// `bzip2.decompress`: the status, `small_footprint` and
/// `expect_concatenated`.
struct Decompressing {
    status: Status,
    small: bool,
    concatenated: bool,
}

/// `bzip2.compress`: the stream, and `is_flushed`, whether no flush is
/// pending since the last bucket.
struct Compressing {
    stream: Compress,
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
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        let mut out = vec![0; BUFFER];
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                if matches!(self.status, Status::Uninitialized) {
                    let Ok(stream) = Decompress::new(self.small) else {
                        return Ok(FilterStatus::ErrFatal);
                    };
                    self.status = Status::Running(stream);
                }
                let Status::Running(stream) = &mut self.status else {
                    total += bucket.len();
                    break;
                };
                let desired = (bucket.len() - bin).min(BUFFER);
                let step = stream.decompress(&bucket[bin..bin + desired], &mut out);
                match step.code {
                    Code::StreamEnd => {
                        self.status = if self.concatenated {
                            Status::Uninitialized
                        } else {
                            Status::Finished
                        };
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
                    output.push_back(out[..step.written].to_vec());
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
                let step = stream.decompress(&[], &mut out);
                if step.written > 0 {
                    output.push_back(out[..step.written].to_vec());
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
}

impl StreamFilter for Compressing {
    /// `php_bz2_compress_filter`.
    fn filter(
        &mut self,
        _req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let action = match flags {
            FilterFlags::FlushClose => Action::Finish,
            FilterFlags::FlushInc => Action::Flush,
            FilterFlags::Normal => Action::Run,
        };
        let mut exit = FilterStatus::FeedMe;
        let mut total = 0;
        let mut out = vec![0; BUFFER];
        while let Some(bucket) = input.pop_front() {
            let mut bin = 0;
            while bin < bucket.len() {
                let desired = (bucket.len() - bin).min(BUFFER);
                self.flushed = action != Action::Run;
                let step = self
                    .stream
                    .compress(&bucket[bin..bin + desired], &mut out, action);
                if !matches!(step.code, Code::RunOk | Code::FlushOk | Code::FinishOk) {
                    return Ok(FilterStatus::ErrFatal);
                }
                total += step.read;
                bin += step.read;
                if step.written > 0 {
                    output.push_back(out[..step.written].to_vec());
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
                let step = self.stream.compress(&[], &mut out, last);
                self.flushed = true;
                if step.written > 0 {
                    output.push_back(out[..step.written].to_vec());
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
pub(crate) fn factory(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
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
        })));
    }
    if !name.eq_ignore_ascii_case(b"bzip2.compress") {
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
    Ok(Compress::new(blocks, work).ok().map(|stream| {
        Box::new(Compressing {
            stream,
            flushed: true,
        }) as Box<dyn StreamFilter>
    }))
}

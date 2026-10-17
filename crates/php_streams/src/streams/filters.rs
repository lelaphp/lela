//! A stream's two filter chains (design §4.3 seam 5): php's
//! `main/streams/filter.c` and the filtered arms of `streams.c`'s read,
//! write and flush. A filter is an extension's [`StreamFilter`], made by
//! the factory registered under its name or a wildcard over it
//! (`php_stream_filter_create`), attached at the head or the tail of the
//! read or the write chain, and named by a resource of the `stream
//! filter` type while it is attached. Every read of the kind passes
//! through the read chain on its way into the read buffer; every write
//! passes through the write chain on its way to the kind, and a flush
//! drains it; the stream's free flushes the write chain with
//! `PSFS_FLAG_FLUSH_CLOSE`, closes each filter's resource and serves each
//! filter's `dtor`.
//!
//! A filter may serve PHP code. What that code throws is kept as the
//! request's pending throwable for the native that asked for the
//! operation, the call answering `PSFS_ERR_FATAL`, as php's
//! `userfilter_filter` answers with `EG(exception)` set; no filter is
//! called while one is pending, as `zend_call_function` calls nothing
//! then.

use std::collections::VecDeque;

use super::PhpStream;
use crate::api::raising::{Thrown, warning};
use crate::api::request::{
    Brigade, FilterChain as Chain, FilterFactory, FilterFlags, FilterStatus, Request, Stream,
    StreamFilter, Supplier, Whence,
};
use crate::api::values::{Emalloc, Owner, Resource, Value};
use crate::state::Layer;
use crate::streams::StreamLayer;

/// `sizeof(php_stream_filter)` on the oracle's LP64, what
/// `_php_stream_filter_alloc` (`Z/main/streams/filter.c:252`) asks
/// `pemalloc` for.
const HEADER: usize = 80;

/// `PHP_STREAM_FILTER_READ` and `PHP_STREAM_FILTER_WRITE`.
const FILTER_READ: i64 = 1;
const FILTER_WRITE: i64 = 2;

/// One filter on a chain: the extension's filter and, once
/// `stream_filter_append` named it, its resource, of which the chain
/// holds one owner.
pub(crate) struct Attached {
    ops: Box<dyn StreamFilter>,
    /// The filter's `php_stream_filter` on the request's heap, as
    /// `pemalloc` keeps one for a stream that is not persistent, freed
    /// where `php_stream_filter_free` frees it; `None` for a persistent
    /// stream's, which php keeps on `malloc`, and for one with a
    /// resource, whose payload it is.
    header: Option<Emalloc>,
    /// The filter's `stream filter` resource, the chain's owner of it,
    /// where one was made.
    res: Option<Owner<Resource>>,
}

/// The payload of a `stream filter` resource: the stream the filter is
/// attached to and the chain it is on; the resource itself finds the
/// filter on it.
struct Named {
    stream: Stream,
    chain: Chain,
}

/// One filter's call on the stream it is attached to. The stream is out
/// of its resource while the filter runs, so what php's filter reaches
/// through its `stream` pointer, `php_stream_tell` and
/// `php_stream_seek` as `consumed` calls them and `php_stream_eof`, is
/// answered from here: the position and the end as they stood when the
/// call was made, and a seek the filter asks for, taken on the stream
/// when the call returns.
///
/// ```php
/// class F extends php_user_filter {
///     function filter($in, $out, &$consumed, $closing): int {
///         echo "at ", ftell($this->stream), $closing ? " closing" : "", "\n";
///         while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
///         if ($closing) fseek($this->stream, 3);
///         return PSFS_PASS_ON;
///     }
/// }
/// stream_filter_register('f', 'F');
/// $m = fopen('php://memory', 'w+'); fwrite($m, "abcdef"); rewind($m); fread($m, 1);
/// $f = stream_filter_append($m, 'f', STREAM_FILTER_READ);
/// var_dump(fread($m, 2));
/// stream_filter_remove($f);
/// var_dump(ftell($m), fread($m, 10));
/// ```
/// ```expect
/// at 1
/// string(2) "bc"
/// at 3 closing
/// int(3)
/// string(3) "def"
/// ```
///
/// `feof` from inside the call answers what the stream was when the
/// call was made, at its end for the last read:
///
/// ```php
/// class F extends php_user_filter {
///     function filter($in, $out, &$consumed, $closing): int {
///         while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
///         echo "filtered", $closing ? " closing" : "", feof($this->stream) ? " at eof" : "", "\n";
///         return PSFS_PASS_ON;
///     }
/// }
/// stream_filter_register('f', 'F');
/// $t = fopen('php://temp', 'r+b'); fwrite($t, str_repeat('a', 8200)); rewind($t);
/// stream_filter_append($t, 'f', STREAM_FILTER_READ);
/// var_dump(strlen(stream_get_contents($t)));
/// ```
/// ```expect
/// filtered
/// filtered closing at eof
/// int(8200)
/// ```
pub(crate) struct FilterCall {
    stream: Stream,
    position: i64,
    /// `php_stream_eof` as it stood when the call was made: nothing in
    /// the read buffer and the kind at its end.
    eof: bool,
    seek: Option<(i64, Option<Whence>)>,
}

impl FilterCall {
    /// The position of `stream`, where it is this call's.
    pub(crate) fn position_of(&self, stream: Stream) -> Option<i64> {
        let position = match self.seek {
            Some((offset, Some(Whence::Set))) => offset,
            _ => self.position,
        };
        (stream.resource() == self.stream.resource()).then_some(position)
    }

    /// Whether `stream`, where it is this call's, was at its end.
    pub(crate) fn eof_of(&self, stream: Stream) -> Option<bool> {
        (stream.resource() == self.stream.resource()).then_some(self.eof)
    }

    /// A seek of `stream` kept for the call's return, `false` where the
    /// stream is not this call's.
    pub(crate) fn defer_seek(
        &mut self,
        stream: Stream,
        offset: i64,
        whence: Option<Whence>,
    ) -> bool {
        if stream.resource() != self.stream.resource() {
            return false;
        }
        self.seek = Some((offset, whence));
        true
    }
}

/// A throwable a filter raised, kept for the native that asked, unless
/// one is kept already.
pub(super) fn defer(req: &mut Request, thrown: Thrown) {
    if req.stream_thrown().is_none() {
        *req.stream_thrown() = Some(thrown);
    } else {
        // A second throwable is given up; a fatal or an exit holds none.
        match crate::api::raising::clear(req, thrown) {
            Ok(()) | Err(_) => {}
        }
    }
}

impl PhpStream {
    fn chain_mut(&mut self, chain: Chain) -> &mut Vec<Attached> {
        match chain {
            Chain::Read => &mut self.readfilters,
            Chain::Write => &mut self.writefilters,
        }
    }

    /// One `fops->filter` call of the filter at `at` of `chain`, the
    /// stream being this one: `PSFS_ERR_FATAL` for a filter that threw,
    /// or that is not called because a throwable is pending already.
    fn run_filter(
        &mut self,
        req: &mut Request,
        chain: Chain,
        at: usize,
        io: (&mut Brigade, &mut Brigade),
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> FilterStatus {
        if req.stream_thrown().is_some() {
            return FilterStatus::ErrFatal;
        }
        let Some(stream) = self.res.map(Stream::of) else {
            return FilterStatus::ErrFatal;
        };
        let position = self.position;
        let eof = self.eof && self.writepos == self.readpos;
        let Some(filter) = self.chain_mut(chain).get_mut(at) else {
            return FilterStatus::ErrFatal;
        };
        let (input, output) = io;
        let call = FilterCall {
            stream,
            position,
            eof,
            seek: None,
        };
        let outer = req.streams().filtering.replace(call);
        req.streams().held_streams.push(stream.resource());
        let answered = filter
            .ops
            .filter(req, stream, input, output, consumed, flags);
        req.streams().held_streams.pop();
        let call = std::mem::replace(&mut req.streams().filtering, outer);
        let status = match answered {
            Ok(status) => status,
            Err(thrown) => {
                defer(req, thrown);
                FilterStatus::ErrFatal
            }
        };
        if let Some((offset, whence)) = call.and_then(|call| call.seek) {
            self.seek(req, offset, whence);
        }
        status
    }

    /// `_php_stream_fill_read_buffer`'s filtered arm
    /// (`Z/main/streams/streams.c:571`): while the stream is not at eof
    /// and the buffer holds fewer than `size` bytes, at most a chunk, one
    /// chunk read from the kind and wound through the chain, the last
    /// read with `PSFS_FLAG_FLUSH_CLOSE` once the kind is at eof and an
    /// empty one with `PSFS_FLAG_FLUSH_INC`; what the chain passes on
    /// goes into the buffer, what a filter keeps waits for the next
    /// round, and `PSFS_ERR_FATAL` breaks the stream. `false` is a read
    /// that failed before a byte was had, or the break.
    pub(super) fn fill_filtered(&mut self, req: &mut Request, size: usize) -> bool {
        let to_read_now = size.min(self.chunk_size());
        let mut chunk = vec![0u8; self.chunk_size()];
        let mut inp = Brigade::new();
        let mut outp = Brigade::new();
        while !self.eof && self.writepos - self.readpos < to_read_now {
            let Self {
                ops,
                eof,
                has_buffered_data,
                ..
            } = self;
            let justread = ops.read_after_buffer(req, &mut chunk, eof, *has_buffered_data);
            self.count_progress(req, justread.unwrap_or(0));
            if justread.is_none() && self.writepos == self.readpos {
                return false;
            }
            let flags = match justread {
                Some(n) if n > 0 => {
                    inp.push_back(chunk[..n].to_vec());
                    if self.eof {
                        FilterFlags::FlushClose
                    } else {
                        FilterFlags::Normal
                    }
                }
                _ if self.eof => FilterFlags::FlushClose,
                _ => FilterFlags::FlushInc,
            };
            let mut status = FilterStatus::ErrFatal;
            for at in 0..self.readfilters.len() {
                status = self.run_filter(req, Chain::Read, at, (&mut inp, &mut outp), None, flags);
                if status != FilterStatus::PassOn {
                    break;
                }
                std::mem::swap(&mut inp, &mut outp);
                outp.clear();
            }
            match status {
                FilterStatus::PassOn => {
                    while let Some(bucket) = inp.pop_front() {
                        if !self.buffer_bucket(req, &bucket) {
                            return false;
                        }
                    }
                }
                FilterStatus::FeedMe => outp.clear(),
                FilterStatus::ErrFatal => {
                    self.eof = true;
                    self.fatal_error = true;
                    return false;
                }
            }
            if justread.is_none_or(|n| n == 0) {
                break;
            }
        }
        true
    }

    /// One bucket appended to the read buffer, the buffer compacted first
    /// where it has not the room and grown where compacting was not
    /// enough; `false` is the memory limit's fatal, rendered.
    fn buffer_bucket(&mut self, req: &mut Request, bucket: &[u8]) -> bool {
        if !self.readbuf.is_empty() && self.readbuf.len() - self.writepos < bucket.len() {
            self.readbuf.copy_within(self.readpos..self.writepos, 0);
            self.writepos -= self.readpos;
            self.readpos = 0;
        }
        self.append_read(req, bucket)
    }

    /// One bucket written after `writepos`, the buffer grown by the
    /// bucket's length where it has not the room, as php's
    /// `readbuflen += bucket->buflen` and `perealloc` grow it; `false` is
    /// the memory limit's fatal, rendered.
    fn append_read(&mut self, req: &mut Request, bucket: &[u8]) -> bool {
        if self.readbuf.len() - self.writepos < bucket.len() {
            let grown = self.readbuf.len() + bucket.len();
            if self
                .readbuf
                .resize(req, grown, self.flags.persistent)
                .is_err()
            {
                return false;
            }
        }
        self.readbuf[self.writepos..self.writepos + bucket.len()].copy_from_slice(bucket);
        self.writepos += bucket.len();
        true
    }

    /// `_php_stream_write_filtered` (`Z/main/streams/streams.c:1245`):
    /// `buf`, where there is one, as one bucket through the write chain,
    /// what it passes on written to the kind; the bytes the head filter
    /// says it consumed, `None` for php's `-1` where the chain broke or
    /// the kind refused a write.
    pub(super) fn write_filtered(
        &mut self,
        req: &mut Request,
        buf: Option<&[u8]>,
        flags: FilterFlags,
    ) -> Option<usize> {
        let mut inp = Brigade::new();
        let mut outp = Brigade::new();
        if let Some(buf) = buf {
            inp.push_back(buf.to_vec());
        }
        let mut consumed = 0usize;
        let mut status = FilterStatus::ErrFatal;
        for at in 0..self.writefilters.len() {
            let counted = (at == 0).then_some(&mut consumed);
            status = self.run_filter(req, Chain::Write, at, (&mut inp, &mut outp), counted, flags);
            if status != FilterStatus::PassOn {
                break;
            }
            std::mem::swap(&mut inp, &mut outp);
            outp.clear();
        }
        let mut failed = false;
        match status {
            FilterStatus::PassOn => {
                while let Some(bucket) = inp.pop_front() {
                    if self.write_buffer(req, &bucket).is_none() {
                        failed = true;
                    }
                }
            }
            FilterStatus::ErrFatal => failed = true,
            FilterStatus::FeedMe => {}
        }
        (!failed).then_some(consumed)
    }

    /// `php_stream_filter_append_ex` and `php_stream_filter_prepend_ex`
    /// (`Z/main/streams/filter.c:264`): the filter put at the tail or the
    /// head of the chain; one appended to the read chain while the read
    /// buffer holds bytes has them wound through it at once, their
    /// filtered bytes replacing the buffer, the buffer given to a filter
    /// that asks to be fed, and php's `Filter failed to process
    /// pre-buffered data` and `false` where it breaks.
    fn attach(&mut self, req: &mut Request, chain: Chain, filter: Attached, append: bool) -> bool {
        if !append {
            self.chain_mut(chain).insert(0, filter);
            return true;
        }
        self.chain_mut(chain).push(filter);
        let held = self.writepos - self.readpos;
        if chain != Chain::Read || held == 0 {
            return true;
        }
        let mut inp: Brigade = VecDeque::from([self.readbuf[self.readpos..self.writepos].to_vec()]);
        let mut outp = Brigade::new();
        let mut consumed = 0usize;
        let at = self.readfilters.len() - 1;
        let mut status = self.run_filter(
            req,
            Chain::Read,
            at,
            (&mut inp, &mut outp),
            Some(&mut consumed),
            FilterFlags::Normal,
        );
        if self.readpos + consumed > self.writepos {
            status = FilterStatus::ErrFatal;
        }
        match status {
            FilterStatus::ErrFatal => {
                let text = format!(
                    "{}(): Filter failed to process pre-buffered data",
                    req.active_function_name()
                );
                warning(req, &text);
                return false;
            }
            FilterStatus::FeedMe => {
                self.readpos = 0;
                self.writepos = 0;
            }
            FilterStatus::PassOn => {
                self.readpos = 0;
                self.writepos = 0;
                while let Some(bucket) = outp.pop_front() {
                    if !self.append_read(req, &bucket) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// `_php_stream_filter_flush` (`Z/main/streams/filter.c:360`): from the
    /// filter at `at` to the chain's tail, each called with nothing new,
    /// the first with `PSFS_FLAG_FLUSH_CLOSE` under `finish` and
    /// `PSFS_FLAG_FLUSH_INC` otherwise; a filter asking to be fed ends the
    /// flush, and one that breaks is `false`. What the last passes on goes
    /// into the read buffer for the read chain, php's shuffle of the
    /// buffer with it, and to the kind for the write chain.
    fn flush_from(&mut self, req: &mut Request, chain: Chain, at: usize, finish: bool) -> bool {
        let mut inp = Brigade::new();
        let mut outp = Brigade::new();
        let mut flags = if finish {
            FilterFlags::FlushClose
        } else {
            FilterFlags::FlushInc
        };
        let len = self.chain_mut(chain).len();
        for current in at..len {
            match self.run_filter(req, chain, current, (&mut inp, &mut outp), None, flags) {
                FilterStatus::FeedMe => return true,
                FilterStatus::ErrFatal => return false,
                FilterStatus::PassOn => {}
            }
            std::mem::swap(&mut inp, &mut outp);
            outp.clear();
            flags = FilterFlags::Normal;
        }
        let flushed: usize = inp.iter().map(Vec::len).sum();
        if flushed == 0 {
            return true;
        }
        match chain {
            Chain::Read => {
                if self.readpos > 0 {
                    // php moves the held bytes to the front and sets
                    // `readpos` to zero before it subtracts it from
                    // `writepos`, which it therefore leaves where it was.
                    self.readbuf.copy_within(self.readpos..self.writepos, 0);
                    self.readpos = 0;
                }
                if flushed > self.readbuf.len() - self.writepos {
                    let grown = self.writepos + flushed + self.chunk_size();
                    if self
                        .readbuf
                        .resize(req, grown, self.flags.persistent)
                        .is_err()
                    {
                        return false;
                    }
                }
                while let Some(bucket) = inp.pop_front() {
                    self.readbuf[self.writepos..self.writepos + bucket.len()]
                        .copy_from_slice(&bucket);
                    self.writepos += bucket.len();
                }
            }
            Chain::Write => {
                while let Some(bucket) = inp.pop_front() {
                    let Self { ops, position, .. } = self;
                    if let Some(count) = ops.write(req, &bucket)
                        && count > 0
                    {
                        *position += i64::try_from(count).unwrap_or(i64::MAX);
                        self.count_progress(req, count);
                    }
                }
            }
        }
        true
    }

    /// The filters of both chains taken off at the stream's free
    /// (`Z/main/streams/streams.c:510`): each one's resource closed and
    /// the chain's owner of it given up, then its `dtor` served.
    pub(super) fn free_filters(&mut self, req: &mut Request) {
        let filters: Vec<Attached> = self
            .readfilters
            .drain(..)
            .chain(self.writefilters.drain(..))
            .collect();
        for filter in filters {
            release_filter(req, filter);
        }
    }
}

/// The filter just attached to `chain`: its tail, `append`, or its head.
fn placed(s: &mut PhpStream, chain: Chain, append: bool) -> Option<&mut Attached> {
    let filters = s.chain_mut(chain);
    let at = if append {
        filters.len().checked_sub(1)?
    } else {
        0
    };
    filters.get_mut(at)
}

/// `php_stream_filter_remove(filter, 1)` past the unlinking: the
/// filter's resource closed, where it has one, and the chain's owner of
/// it given up, then `php_stream_filter_free`'s `dtor`, served whatever
/// a call before it threw, as php's is: what a filter holds is given up
/// with it, and a filter whose `dtor` serves PHP code serves none while
/// a throwable is pending ([`crate::api::request::StreamFilter::close`]).
fn release_filter(req: &mut Request, filter: Attached) {
    let Attached {
        mut ops,
        header,
        res,
    } = filter;
    if let Some(res) = res {
        res.close(req);
        res.give_up(req);
    }
    if let Err(thrown) = ops.close(req) {
        defer(req, thrown);
    }
    if let Some(header) = header {
        header.free(req);
    }
}

pub(crate) trait FilterLayer {
    fn filter_create(
        &mut self,
        name: &[u8],
        params: Option<&Value>,
        persistent: bool,
    ) -> Option<Attached>;
    fn remove_filter(&mut self, res: Resource) -> Result<Option<bool>, Thrown>;
}

impl FilterLayer for Request {
    /// `php_stream_filter_create` (`Z/main/streams/filter.c:212`): the
    /// factory registered under `name`, else under the name with its
    /// last segment and then each shorter one made `*`, asked for a
    /// filter; php's `Unable to locate filter "%s"` where no factory
    /// matched and `Unable to create or locate filter "%s"` where the
    /// last one that did refused. `persistent` is the stream's, which
    /// the factory is told and which keeps the filter's
    /// `php_stream_filter` off the request's heap.
    ///
    /// A filter's `php_stream_filter` is counted from its making to its
    /// free, where php's `_php_stream_filter_alloc` and
    /// `php_stream_filter_free` count it, beside what its factory makes.
    ///
    /// ```php
    /// foreach (['string.rot13', 'zlib.deflate', 'bzip2.compress'] as $name) {
    ///     $f = fopen('php://memory', 'w+');
    ///     $w = fopen('php://memory', 'w+'); stream_filter_append($w, $name, STREAM_FILTER_WRITE);
    ///     $m = memory_get_usage(); $h = stream_filter_append($f, $name, STREAM_FILTER_READ); $made = memory_get_usage() - $m;
    ///     $m = memory_get_usage(); stream_filter_remove($h); echo $name, ' ', $made, ' ', memory_get_usage() - $m, "\n";
    /// }
    /// ```
    /// ```expect
    /// string.rot13 272 -48
    /// zlib.deflate 467184 -454896
    /// bzip2.compress 3531920 -3518672
    /// ```
    fn filter_create(
        &mut self,
        name: &[u8],
        params: Option<&Value>,
        persistent: bool,
    ) -> Option<Attached> {
        let lookup = |req: &Request, wanted: &[u8]| -> Option<(FilterFactory, Supplier)> {
            req.streams_ref()?
                .filters
                .iter()
                .find(|(n, _)| n.as_slice() == wanted)
                .map(|(_, f)| (f.row, f.by))
        };
        // A factory that threw is php's `create_filter` answering `NULL`
        // with `EG(exception)` set: refused, and the throwable kept for
        // the native.
        let create = |req: &mut Request, (found, by): (FilterFactory, Supplier)| match req
            .serve_supplied(by, |req| found(req, name, params, persistent))
        {
            Ok(filter) => filter.map(|filter| crate::supplied::filter(by, filter)),
            Err(thrown) => {
                defer(req, thrown);
                None
            }
        };
        let mut factory = lookup(self, name);
        let mut filter = None;
        if let Some(found) = factory {
            filter = create(self, found);
        } else if name.contains(&b'.') {
            let mut wild = name.to_vec();
            while let Some(period) = wild.iter().rposition(|b| *b == b'.') {
                wild.truncate(period + 1);
                wild.push(b'*');
                if let Some(found) = lookup(self, &wild) {
                    factory = Some(found);
                    filter = create(self, found);
                    if filter.is_some() {
                        break;
                    }
                }
                wild.truncate(period);
            }
        }
        if filter.is_none() {
            let what = if factory.is_none() {
                "Unable to locate filter"
            } else {
                "Unable to create or locate filter"
            };
            let text = format!(
                "{}(): {what} \"{}\"",
                self.active_function_name(),
                String::from_utf8_lossy(name)
            );
            warning(self, &text);
        }
        let ops = filter?;
        let header = if persistent {
            None
        } else {
            match Emalloc::new(self, HEADER) {
                Ok(header) => Some(header),
                Err(thrown) => {
                    defer(self, thrown);
                    return None;
                }
            }
        };
        Some(Attached {
            ops,
            header,
            res: None,
        })
    }

    fn remove_filter(&mut self, res: Resource) -> Result<Option<bool>, Thrown> {
        if res.kind() != Some(self.types().filter) {
            return Ok(None);
        }
        let Some((stream, chain)) = res.native::<Named>(self).map(|n| (n.stream, n.chain)) else {
            return Ok(None);
        };
        let flushed = self
            .stream_op(stream, |req, s| {
                let at = s
                    .chain_mut(chain)
                    .iter()
                    .position(|f| f.res.as_deref() == Some(&res))?;
                Some(s.flush_from(req, chain, at, true))
            })
            .flatten();
        if flushed == Some(false) {
            let text = format!(
                "{}(): Unable to flush filter, not removing",
                self.active_function_name()
            );
            warning(self, &text);
            return Ok(Some(false));
        }
        let taken = self
            .stream_op(stream, |_, s| {
                let filters = s.chain_mut(chain);
                let at = filters
                    .iter()
                    .position(|f| f.res.as_deref() == Some(&res))?;
                Some(filters.remove(at))
            })
            .flatten();
        if let Some(filter) = taken {
            release_filter(self, filter);
        } else {
            res.close(self);
        }
        match self.stream_thrown().take() {
            Some(thrown) => Err(thrown),
            None => Ok(Some(true)),
        }
    }
}

impl Stream {
    /// `php_stream_filter_create(name, NULL, persistent)` and
    /// `php_stream_filter_append` on `chain`, what
    /// `php_stream_apply_filter_list` makes of one name
    /// (`Z/ext/standard/php_fopen_wrapper.c:153`): a filter made by the
    /// factory `name` finds, with no parameters, at the tail of the chain
    /// with no resource of its own. `false` is a filter that could not be
    /// made, after the filter layer's warning; the `Err` is what a filter
    /// threw over the bytes already read.
    pub fn append_filter(
        self,
        req: &mut Request,
        name: &[u8],
        chain: Chain,
    ) -> Result<bool, Thrown> {
        let persistent = req.stream_ref(self).is_some_and(|s| s.flags.persistent);
        let Some(filter) = req.filter_create(name, None, persistent) else {
            return Ok(false);
        };
        req.stream_op(self, |req, s| s.attach(req, chain, filter, true));
        match req.stream_thrown().take() {
            Some(thrown) => Err(thrown),
            None => Ok(true),
        }
    }

    /// `apply_filter_to_stream` (`Z/ext/standard/streamsfuncs.c:1208`)
    /// past its parameters: `read_write`, `STREAM_FILTER_READ`,
    /// `STREAM_FILTER_WRITE` or both, or neither for the chains the
    /// stream's `mode` reads and writes; a filter made for each chain by
    /// the factory `name` finds, each with `params`, and attached at the
    /// tail, `append`, or the head. The answer is a new `stream filter`
    /// resource for the last filter attached, one owner the caller's and
    /// one the chain's, and `None` where a filter could not be made or
    /// the pre-buffered bytes broke it, after php's warning.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn add_filter(
        self,
        req: &mut Request,
        name: &[u8],
        params: Option<&Value>,
        read_write: i64,
        append: bool,
    ) -> Result<Option<Owner<Resource>>, Thrown> {
        let mut read_write = read_write;
        if read_write & (FILTER_READ | FILTER_WRITE) == 0 {
            let mode = req
                .stream_ref(self)
                .map(|s| s.mode.bytes().to_vec())
                .unwrap_or_default();
            if mode.contains(&b'r') || mode.contains(&b'+') {
                read_write |= FILTER_READ;
            }
            if mode.iter().any(|b| matches!(b, b'w' | b'+' | b'a')) {
                read_write |= FILTER_WRITE;
            }
        }
        let persistent = req.stream_ref(self).is_some_and(|s| s.flags.persistent);
        let mut last = None;
        for (bit, chain) in [(FILTER_READ, Chain::Read), (FILTER_WRITE, Chain::Write)] {
            if read_write & bit == 0 {
                continue;
            }
            let Some(filter) = req.filter_create(name, params, persistent) else {
                return match req.stream_thrown().take() {
                    Some(thrown) => Err(thrown),
                    None => Ok(None),
                };
            };
            let attached = req
                .stream_op(self, |req, s| s.attach(req, chain, filter, append))
                .unwrap_or(false);
            if !attached {
                // `php_stream_filter_remove(filter, 1)`: the filter that
                // broke on the buffer taken off its chain and freed.
                let taken = req
                    .stream_op(self, |_, s| s.chain_mut(chain).pop())
                    .flatten();
                if let Some(filter) = taken {
                    release_filter(req, filter);
                }
                return match req.stream_thrown().take() {
                    Some(thrown) => Err(thrown),
                    None => Ok(None),
                };
            }
            last = Some(chain);
        }
        let Some(chain) = last else {
            return Ok(None);
        };
        // php's resource is the filter itself, `zend_register_resource(filter,
        // le_stream_filter)`: the last filter's `php_stream_filter` is
        // laid down as the resource's payload, where the header it was
        // made with goes.
        let header = req
            .stream_op(self, |_, s| {
                placed(s, chain, append).and_then(|f| f.header.take())
            })
            .flatten();
        let kind = req.types().filter;
        let named = Named {
            stream: self,
            chain,
        };
        let res = match header {
            Some(header) => {
                header.free(req);
                Resource::sized(req, kind, named, HEADER)?
            }
            None => Resource::persistent(req, kind, named, HEADER)?,
        };
        let mut held = Some(Owner::take(req, *res));
        req.stream_op(self, |_, s| {
            if let Some(filter) = placed(s, chain, append) {
                filter.res = held.take();
            }
        });
        if let Some(unheld) = held {
            unheld.give_up(req);
        }
        match req.stream_thrown().take() {
            Some(thrown) => Err(thrown),
            None => Ok(Some(res)),
        }
    }
}

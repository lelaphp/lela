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

use super::{PhpStream, StreamOp};
use crate::api::raising::{Thrown, fatal, warning};
use crate::api::request::{
    Brigade, FilterChain as Chain, FilterFactory, FilterFlags, FilterStatus, KeptFilter,
    MadeFilter, Request, Stream, StreamFilter, Supplier,
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

/// One filter on a chain: the extension's filter, of the type its chain
/// holds ([`Chains`]), and, once `stream_filter_append` named it, its
/// resource, of which the chain holds one owner.
pub(crate) struct Attached<F: ?Sized> {
    ops: FilterOps<F>,
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

/// `filter->fops` with what the factory made: on its chain, or lent out
/// for its call ([`StreamOp::run_filter`]), the place on the chain marked with
/// the call's number so the filter comes back to it wherever PHP code
/// the call served moved the chain.
enum FilterOps<F: ?Sized> {
    On(Box<F>),
    Lent(Lend),
}

impl<F: ?Sized> FilterOps<F> {
    /// The filter, where it is on its chain and not out for a call.
    fn into_on(self) -> Option<Box<F>> {
        match self {
            FilterOps::On(ops) => Some(ops),
            FilterOps::Lent(_) => None,
        }
    }
}

/// A filter its factory made and the layer has not put on a chain yet,
/// with its header: what [`FilterLayer::filter_create`] answers.
pub(crate) struct Made {
    ops: MadeFilter,
    header: Option<Emalloc>,
}

/// A filter taken off its chain, and what of it is still to release.
struct Off {
    ops: Option<MadeFilter>,
    header: Option<Emalloc>,
    res: Option<Owner<Resource>>,
}

/// A stream's two filter chains, `stream->readfilters` and
/// `stream->writefilters`, head first: every read of the kind passes
/// through the first, every write through the second before the kind.
/// Typed by the stream they are on: a persistent stream's hold filters it
/// may keep past the request alone ([`KeptFilter`]), so what
/// [`PhpStream::keep_filters`] keeps of them is engine state by type, and
/// every other stream's hold any filter.
pub(crate) enum Chains {
    Request(Pair<dyn StreamFilter>),
    Kept(Pair<dyn KeptFilter>),
}

impl Chains {
    /// No filters yet, of the type a stream of `persistent` holds.
    fn new(persistent: bool) -> Chains {
        if persistent {
            Chains::Kept(Pair::default())
        } else {
            Chains::Request(Pair::default())
        }
    }
}

/// `body` over the read and write chains `chains` holds, whichever type
/// of filter they hold, as `pair`.
macro_rules! pair {
    ($chains:expr, $pair:ident => $body:expr) => {
        match $chains {
            Chains::Request($pair) => $body,
            Chains::Kept($pair) => $body,
        }
    };
}

/// The read and the write chain of one type of filter.
pub(crate) struct Pair<F: ?Sized> {
    read: Vec<Attached<F>>,
    write: Vec<Attached<F>>,
}

impl<F: ?Sized> Default for Pair<F> {
    fn default() -> Pair<F> {
        Pair {
            read: Vec::new(),
            write: Vec::new(),
        }
    }
}

/// The type of filter one type of chain holds, and the [`MadeFilter`]
/// each one is.
pub(crate) trait OnChain {
    /// `made` as the chain holds it, or handed back where it may not
    /// stand there.
    fn hold(made: MadeFilter) -> Result<Box<Self>, MadeFilter>;

    /// What a filter of the chain is, taken off it.
    fn made(on: Box<Self>) -> MadeFilter;
}

impl OnChain for dyn StreamFilter {
    fn hold(made: MadeFilter) -> Result<Box<dyn StreamFilter>, MadeFilter> {
        Ok(match made {
            MadeFilter::Request(filter) => filter,
            MadeFilter::Kept(filter) => filter,
        })
    }

    fn made(on: Box<dyn StreamFilter>) -> MadeFilter {
        MadeFilter::Request(on)
    }
}

impl OnChain for dyn KeptFilter {
    fn hold(made: MadeFilter) -> Result<Box<dyn KeptFilter>, MadeFilter> {
        match made {
            MadeFilter::Kept(filter) => Ok(filter),
            refused @ MadeFilter::Request(_) => Err(refused),
        }
    }

    fn made(on: Box<dyn KeptFilter>) -> MadeFilter {
        MadeFilter::Kept(on)
    }
}

impl<F: ?Sized + OnChain> Attached<F> {
    /// The filter off its chain, its filter as it was made.
    fn off(self) -> Off {
        Off {
            ops: self.ops.into_on().map(F::made),
            header: self.header,
            res: self.res,
        }
    }
}

impl<F: ?Sized + OnChain> Pair<F> {
    fn chain(&mut self, chain: Chain) -> &mut Vec<Attached<F>> {
        match chain {
            Chain::Read => &mut self.read,
            Chain::Write => &mut self.write,
        }
    }

    fn count(&self, chain: Chain) -> usize {
        match chain {
            Chain::Read => self.read.len(),
            Chain::Write => self.write.len(),
        }
    }

    /// `made` put at the tail of `chain`, `append`, or its head; handed
    /// back where it may not stand on a chain of this type.
    fn place(&mut self, chain: Chain, made: Made, append: bool) -> Result<(), Made> {
        let Made { ops, header } = made;
        let ops = match F::hold(ops) {
            Ok(ops) => ops,
            Err(ops) => return Err(Made { ops, header }),
        };
        let attached = Attached {
            ops: FilterOps::On(ops),
            header,
            res: None,
        };
        let filters = self.chain(chain);
        if append {
            filters.push(attached);
        } else {
            filters.insert(0, attached);
        }
        Ok(())
    }

    /// The filter at `at` of `chain` lent out under `lend`, `None` where
    /// there is none there or it is out already.
    fn lend(&mut self, chain: Chain, at: usize, lend: Lend) -> Option<MadeFilter> {
        let attached = self.chain(chain).get_mut(at)?;
        if matches!(attached.ops, FilterOps::Lent(_)) {
            return None;
        }
        std::mem::replace(&mut attached.ops, FilterOps::Lent(lend))
            .into_on()
            .map(F::made)
    }

    /// `made` back to the place marked `lend` on either chain; handed
    /// back where there is no such place, or it may not stand there.
    fn put(&mut self, lend: Lend, made: MadeFilter) -> Option<MadeFilter> {
        let place = (self.read.iter_mut().chain(self.write.iter_mut()))
            .find(|f| matches!(f.ops, FilterOps::Lent(n) if n == lend));
        let Some(place) = place else {
            return Some(made);
        };
        match F::hold(made) {
            Ok(ops) => {
                place.ops = FilterOps::On(ops);
                None
            }
            Err(made) => Some(made),
        }
    }

    /// Where on `chain` the filter `res` names stands.
    fn position(&mut self, chain: Chain, res: Resource) -> Option<usize> {
        self.chain(chain)
            .iter()
            .position(|f| f.res.as_deref() == Some(&res))
    }

    /// Every filter of both chains taken off, the read chain's first.
    fn off(self) -> Vec<Off> {
        (self.read.into_iter().chain(self.write))
            .map(Attached::off)
            .collect()
    }
}

/// The number of one filter call, which marks the lent filter's place on
/// its chain ([`FilterOps::Lent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Lend(u64);

impl crate::state::State {
    /// The next filter call's number.
    fn next_lend(&mut self) -> Lend {
        self.filter_lends += 1;
        Lend(self.filter_lends)
    }
}

/// The payload of a `stream filter` resource: the stream the filter is
/// attached to and the chain it is on; the resource itself finds the
/// filter on it.
struct Named {
    stream: Stream,
    chain: Chain,
}

/// A throwable a filter raised, kept for the native that asked, unless
/// one is kept already.
pub(super) fn defer(req: &mut Request, thrown: Thrown) {
    if req.diagnostics_mut().stream_thrown().is_none() {
        *req.diagnostics_mut().stream_thrown() = Some(thrown);
    } else {
        // A second throwable is given up; a fatal or an exit holds none.
        match crate::api::raising::clear(req, thrown) {
            Ok(()) | Err(_) => {}
        }
    }
}

impl PhpStream {
    /// The stream's chains, made of the type its persistence holds where
    /// it had none.
    fn chains_mut(&mut self) -> &mut Chains {
        let persistent = self.flags.persistent;
        self.filters
            .get_or_insert_with(|| Box::new(Chains::new(persistent)))
    }

    /// How many filters `chain` has.
    pub(super) fn filter_count(&self, chain: Chain) -> usize {
        (self.filters.as_deref()).map_or(0, |chains| pair!(chains, p => p.count(chain)))
    }

    /// Whether `chain` has a filter.
    pub(super) fn has_filters(&self, chain: Chain) -> bool {
        self.filter_count(chain) > 0
    }

    /// The filter at `at` of `chain` lent out for its call under the
    /// call's number `lend`, `None` where there is none there or it is
    /// out already.
    fn lend_filter(&mut self, chain: Chain, at: usize, lend: Lend) -> Option<MadeFilter> {
        pair!(self.filters.as_deref_mut()?, p => p.lend(chain, at, lend))
    }

    /// A filter back from its call to the place marked `lend`, on
    /// whichever chain it is now; handed back where PHP code the call
    /// served took the place off.
    fn put_filter(&mut self, lend: Lend, ops: MadeFilter) -> Option<MadeFilter> {
        match self.filters.as_deref_mut() {
            Some(chains) => pair!(chains, p => p.put(lend, ops)),
            None => Some(ops),
        }
    }

    /// The filter `res` names taken off `chain`.
    fn take_named(&mut self, chain: Chain, res: Resource) -> Option<Off> {
        pair!(self.filters.as_deref_mut()?, p => {
            let at = p.position(chain, res)?;
            Some(p.chain(chain).remove(at).off())
        })
    }

    /// The filters of both chains taken off and kept, as a persistent
    /// stream keeps them past its resource: each one's resource closed and
    /// the chain's owner of it given up, its `fops` kept, its `dtor` not
    /// served ([`super::persistent::Kept`]). A persistent stream's chains
    /// hold kept filters alone, so each is kept as it stands; a stream of
    /// the request's keeps none past it, each released as its free
    /// releases them. Each chain writes and reads through what it kept
    /// once the stream is asked for again:
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// [$host, $port] = explode(':', stream_socket_get_name($s, false));
    /// $a = pfsockopen($host, $port);
    /// $peer = stream_socket_accept($s, 1);
    /// stream_filter_append($a, 'convert.base64-encode', STREAM_FILTER_WRITE);
    /// stream_filter_append($a, 'string.rot13', STREAM_FILTER_WRITE);
    /// stream_filter_append($a, 'dechunk', STREAM_FILTER_READ);
    /// $a = null;
    /// $b = pfsockopen($host, $port);
    /// fwrite($b, "abc"); usleep(50000);
    /// var_dump(fread($peer, 10));
    /// fwrite($peer, "3\r\nxyz\r\n0\r\n\r\n"); usleep(50000);
    /// var_dump(fread($b, 3));
    /// ```
    /// ```expect
    /// string(4) "LJWw"
    /// string(3) "xyz"
    /// ```
    pub(super) fn keep_filters(&mut self, req: &mut Request) -> super::persistent::Kept {
        let mut kept = super::persistent::Kept::default();
        let Some(chains) = self.filters.take() else {
            return kept;
        };
        let Pair { read, write } = match *chains {
            Chains::Kept(pair) => pair,
            Chains::Request(pair) => {
                for filter in pair.off() {
                    release_filter(req, filter);
                }
                return kept;
            }
        };
        for (chain, into) in [(read, &mut kept.read), (write, &mut kept.write)] {
            for Attached { ops, header, res } in chain {
                if let Some(res) = res {
                    res.close(req);
                    res.give_up(req);
                }
                if let Some(header) = header {
                    header.free(req);
                }
                into.extend(ops.into_on());
            }
        }
        kept
    }

    /// Kept filters attached again, each chain in its order, with no
    /// resource, as the stream php's list kept still has them.
    pub(super) fn attach_kept(&mut self, kept: super::persistent::Kept) {
        fn on<F: ?Sized>(ops: Box<F>) -> Attached<F> {
            Attached {
                ops: FilterOps::On(ops),
                header: None,
                res: None,
            }
        }
        let super::persistent::Kept { read, write } = kept;
        let chains = (self.filters).get_or_insert_with(|| Box::new(Chains::new(true)));
        match &mut **chains {
            Chains::Kept(p) => {
                p.read.extend(read.into_iter().map(on));
                p.write.extend(write.into_iter().map(on));
            }
            Chains::Request(p) => {
                let upcast = |ops: Box<dyn KeptFilter>| on(ops as Box<dyn StreamFilter>);
                p.read.extend(read.into_iter().map(upcast));
                p.write.extend(write.into_iter().map(upcast));
            }
        }
    }

    /// The filters of both chains taken off at the stream's free
    /// (`Z/main/streams/streams.c:510`): each one's resource closed and
    /// the chain's owner of it given up, then its `dtor` served.
    pub(super) fn free_filters(&mut self, req: &mut Request) {
        let Some(chains) = self.filters.take() else {
            return;
        };
        for filter in pair!(*chains, p => p.off()) {
            release_filter(req, filter);
        }
    }
}

impl StreamOp<'_> {
    /// One `fops->filter` call of the filter at `at` of `chain`, the
    /// filter lent out of its chain for the call, or a filter shared for
    /// it where the filter can be re-entered, and the stream where it
    /// is, so what the filter reaches through its `stream`,
    /// `php_stream_tell`, `php_stream_seek` as `consumed` calls them and
    /// `php_stream_eof`, is the live stream, as php's is:
    /// `PSFS_ERR_FATAL` for a filter that threw, for one in use by a call
    /// further out, or one not called because a throwable is pending
    /// already. `fclose` refuses the stream for the call's length, as
    /// `userfilter_filter` sets `PHP_STREAM_FLAG_NO_FCLOSE` around it.
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
    /// `feof` from inside the call answers what the stream is, at its
    /// end for the last read:
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
    ///
    /// A user filter that reads its own stream is called again inside its
    /// call, as deep as it reads ([`StreamFilter::share`]):
    ///
    /// ```php
    /// class F extends php_user_filter {
    ///     static $depth = 0;
    ///     function filter($in, $out, &$consumed, $closing): int {
    ///         $d = ++self::$depth;
    ///         while ($b = stream_bucket_make_writeable($in)) { echo str_repeat('  ', $d - 1), "$d: {$b->data}\n"; $consumed += $b->datalen; $b->data = strtoupper($b->data); stream_bucket_append($out, $b); }
    ///         if ($d < 3 && !$closing) var_dump(fread($this->stream, 2));
    ///         self::$depth--;
    ///         return PSFS_PASS_ON;
    ///     }
    /// }
    /// stream_filter_register('f', 'F');
    /// $m = fopen('php://memory', 'w+'); fwrite($m, 'abcdefghij'); rewind($m);
    /// stream_set_chunk_size($m, 2);
    /// stream_filter_append($m, 'f', STREAM_FILTER_READ);
    /// var_dump(fread($m, 4), ftell($m));
    /// ```
    /// ```expect
    /// 1: ab
    ///   2: cd
    ///     3: ef
    /// string(2) "EF"
    /// string(2) "CD"
    /// 1: gh
    ///   2: ij
    /// string(0) ""
    /// string(2) "IJ"
    /// string(4) "ABGH"
    /// int(10)
    /// ```
    ///
    /// and one that closes it is refused, each call and the close's own
    /// flush alike:
    ///
    /// ```php
    /// class G extends php_user_filter {
    ///     function filter($in, $out, &$consumed, $closing): int {
    ///         if ($this->stream) var_dump(fclose($this->stream));
    ///         while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
    ///         return PSFS_PASS_ON;
    ///     }
    /// }
    /// stream_filter_register('g', 'G');
    /// $w = fopen('php://memory', 'w+');
    /// stream_filter_append($w, 'g', STREAM_FILTER_WRITE);
    /// var_dump(fwrite($w, "xyz"), is_resource($w));
    /// rewind($w); var_dump(stream_get_contents($w));
    /// var_dump(fclose($w), is_resource($w));
    /// ```
    /// ```expect
    /// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 4
    /// bool(false)
    /// int(3)
    /// bool(true)
    ///
    /// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 4
    /// bool(false)
    /// string(3) "xyz"
    ///
    /// Warning: fclose(): cannot close the provided stream, as it must not be manually closed in %s on line 4
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn run_filter(
        &mut self,
        chain: Chain,
        at: usize,
        io: (&mut Brigade, &mut Brigade),
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> FilterStatus {
        if self.req.diagnostics_mut().stream_thrown().is_some() {
            return FilterStatus::ErrFatal;
        }
        let Some(stream) = self.with(|s| s.res.map(Stream::of)).flatten() else {
            return FilterStatus::ErrFatal;
        };
        let lend = self.req.streams().next_lend();
        let Some(filter) = self.with(|s| s.lend_filter(chain, at, lend)).flatten() else {
            return FilterStatus::ErrFatal;
        };
        // A filter that can be re-entered is called through a filter shared
        // for the call, and goes back on its chain at once, where a call
        // made inside this one finds it, as php's finds the same filter.
        let mut called = match filter.as_filter().share(self.req) {
            Some(shared) => {
                self.put_back(lend, filter);
                MadeFilter::Request(shared)
            }
            None => filter,
        };
        let (input, output) = io;
        self.req.streams().held_streams.push(stream.resource());
        let answered =
            (called.as_filter_mut()).filter(self.req, stream, input, output, consumed, flags);
        self.req.streams().held_streams.pop();
        self.put_back(lend, called);
        match answered {
            Ok(status) => status,
            Err(thrown) => {
                defer(self.req, thrown);
                FilterStatus::ErrFatal
            }
        }
    }

    /// How many filters `chain` has as the stream stands now, which PHP
    /// code a filter served may have changed, as php's walk follows
    /// `filter->next` as it stands.
    fn filters_on(&mut self, chain: Chain) -> usize {
        self.with(|s| s.filter_count(chain)).unwrap_or(0)
    }

    /// A filter back from its call to its place on the chain marked
    /// `lend`; one whose place is gone, or shared for the call, closed.
    fn put_back(&mut self, lend: Lend, filter: MadeFilter) {
        let unplaced = self.with(|s| s.put_filter(lend, filter)).flatten();
        if let Some(mut unplaced) = unplaced
            && let Err(thrown) = unplaced.as_filter_mut().close(self.req)
        {
            defer(self.req, thrown);
        }
    }

    /// `_php_stream_fill_read_buffer`'s filtered arm
    /// (`Z/main/streams/streams.c:567`): `chunk_buf` asked for at the
    /// chunk size first, fallibly as php 8.5.12's `femalloc` asks, so a
    /// chunk the memory limit has no room for is a fill that failed and
    /// the limit's fatal is raised after the call; then, while the stream
    /// is not at eof and the buffer holds fewer than `size` bytes, at most
    /// a chunk, one chunk read from the kind and wound through the chain,
    /// the last read with `PSFS_FLAG_FLUSH_CLOSE` once the kind is at eof
    /// and an empty one with `PSFS_FLAG_FLUSH_INC`; what the chain passes
    /// on goes into the buffer, what a filter keeps waits for the next
    /// round, and `PSFS_ERR_FATAL` breaks the stream. `false` is the
    /// chunk refused, a read that failed before a byte was had, or the
    /// break.
    ///
    /// ```php
    /// ini_set('memory_limit', '8M');
    /// $fp = fopen(__FILE__, 'rb');
    /// stream_filter_append($fp, "string.rot13");
    /// stream_set_chunk_size($fp, 100 * 1024 * 1024);
    /// var_dump(fread($fp, 1));
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 104857600 bytes) in %s on line 6
    /// ```
    pub(super) fn fill_filtered(&mut self, size: usize) -> bool {
        let Some(chunk_size) = self.with(|s| s.chunk_size()) else {
            return false;
        };
        let Ok(Some(mut chunk)) = self
            .req
            .memory_mut()
            .fallible(|req| Emalloc::new(req, chunk_size))
        else {
            return false;
        };
        let filled = self.fill_through_chain(size.min(chunk_size), chunk.bytes_mut());
        chunk.free(self.req);
        filled
    }

    /// The rounds of [`StreamOp::fill_filtered`] while the buffer holds
    /// fewer than `to_read_now` bytes, each read of the kind into
    /// `chunk`, php's `chunk_buf`.
    fn fill_through_chain(&mut self, to_read_now: usize, chunk: &mut [u8]) -> bool {
        let mut inp = Brigade::new();
        let mut outp = Brigade::new();
        while self.with(|s| !s.eof && s.writepos - s.readpos < to_read_now) == Some(true) {
            let justread = self.read_kind(chunk);
            self.count_progress(justread.unwrap_or(0));
            let Some((empty, eof)) = self.with(|s| (s.writepos == s.readpos, s.eof)) else {
                return false;
            };
            if justread.is_none() && empty {
                return false;
            }
            let flags = match justread {
                Some(n) if n > 0 => {
                    inp.push_back(chunk[..n].to_vec());
                    if eof {
                        FilterFlags::FlushClose
                    } else {
                        FilterFlags::Normal
                    }
                }
                _ if eof => FilterFlags::FlushClose,
                _ => FilterFlags::FlushInc,
            };
            let mut status = FilterStatus::ErrFatal;
            let mut at = 0;
            while at < self.filters_on(Chain::Read) {
                status = self.run_filter(Chain::Read, at, (&mut inp, &mut outp), None, flags);
                at += 1;
                if status != FilterStatus::PassOn {
                    break;
                }
                std::mem::swap(&mut inp, &mut outp);
                outp.clear();
            }
            match status {
                FilterStatus::PassOn => {
                    while let Some(bucket) = inp.pop_front() {
                        if !self.buffer_bucket(&bucket) {
                            // php 8.5.12 drops the buckets left and goes
                            // on with a buffer that still holds its
                            // bytes; this one is empty, so the fill ends
                            // here and no read waits on the kind for
                            // bytes the limit's fatal discards.
                            return true;
                        }
                    }
                }
                FilterStatus::FeedMe => outp.clear(),
                FilterStatus::ErrFatal => {
                    self.with(|s| {
                        s.eof = true;
                        s.fatal_error = true;
                    });
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
    /// where it has not the room and grown by the bucket's length where
    /// compacting was not enough, `fperealloc` asked fallibly as php
    /// 8.5.12 asks it, since what a filter hands on is unbounded by what
    /// it was fed; `false` is the growth the memory limit refused.
    fn buffer_bucket(&mut self, bucket: &[u8]) -> bool {
        let Some((room, len)) = self.with(|s| {
            if !s.readbuf.is_empty() && s.readbuf.len() - s.writepos < bucket.len() {
                s.compact();
            }
            (s.readbuf.len() - s.writepos, s.readbuf.len())
        }) else {
            return false;
        };
        if room < bucket.len() && !self.grow_readbuf(len + bucket.len()) {
            return false;
        }
        self.with(|s| s.push_read(bucket)) == Some(true)
    }

    /// One bucket written after `writepos`, the buffer grown by the
    /// bucket's length where it has not the room, as php's
    /// `readbuflen += bucket->buflen` and `perealloc` grow it; `false` is
    /// the memory limit's fatal, rendered.
    pub(super) fn append_read(&mut self, bucket: &[u8]) -> bool {
        let Some((room, len)) = self.with(|s| (s.readbuf.len() - s.writepos, s.readbuf.len()))
        else {
            return false;
        };
        if room < bucket.len() && !self.resize_readbuf(len + bucket.len()) {
            return false;
        }
        self.with(|s| s.push_read(bucket)) == Some(true)
    }

    /// `_php_stream_write_filtered` (`Z/main/streams/streams.c:1245`):
    /// `buf`, where there is one, as one bucket through the write chain,
    /// what it passes on written to the kind; the bytes the head filter
    /// says it consumed, `None` for php's `-1` where the chain broke or
    /// the kind refused a write.
    pub(super) fn write_filtered(
        &mut self,
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
        let mut at = 0;
        while at < self.filters_on(Chain::Write) {
            let counted = (at == 0).then_some(&mut consumed);
            status = self.run_filter(Chain::Write, at, (&mut inp, &mut outp), counted, flags);
            at += 1;
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
                    if self.write_buffer(&bucket).is_none() {
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
    /// pre-buffered data` and [`Attach::Broke`] where it breaks. A filter
    /// of the request's is refused a persistent stream's chain
    /// ([`Chains`]): released, and an `InternalError`, since its factory
    /// was told the stream is persistent.
    fn attach(&mut self, chain: Chain, filter: Made, append: bool) -> Attach {
        let placed = self.with(|s| -> Result<_, Made> {
            pair!(s.chains_mut(), p => p.place(chain, filter, append))?;
            let held = &s.readbuf[s.readpos..s.writepos];
            Ok((append && chain == Chain::Read && !held.is_empty())
                .then(|| (held.to_vec(), s.filter_count(Chain::Read) - 1)))
        });
        let (held, at) = match placed {
            Some(Ok(Some(held))) => held,
            Some(Ok(None)) | None => return Attach::On,
            Some(Err(refused)) => {
                release_filter(
                    self.req,
                    Off {
                        ops: Some(refused.ops),
                        header: refused.header,
                        res: None,
                    },
                );
                let thrown = fatal(
                    self.req,
                    "InternalError: a filter factory answered a filter of the request's for a persistent stream",
                );
                defer(self.req, thrown);
                return Attach::Refused;
            }
        };
        let held_len = held.len();
        let mut inp: Brigade = VecDeque::from([held]);
        let mut outp = Brigade::new();
        let mut consumed = 0usize;
        let mut status = self.run_filter(
            Chain::Read,
            at,
            (&mut inp, &mut outp),
            Some(&mut consumed),
            FilterFlags::Normal,
        );
        if consumed > held_len {
            status = FilterStatus::ErrFatal;
        }
        match status {
            FilterStatus::ErrFatal => {
                let text = format!(
                    "{}(): Filter failed to process pre-buffered data",
                    self.req.frames().active_function_name()
                );
                warning(self.req, &text);
                return Attach::Broke;
            }
            FilterStatus::FeedMe => {
                self.with(PhpStream::drop_buffer);
            }
            FilterStatus::PassOn => {
                self.with(PhpStream::drop_buffer);
                while let Some(bucket) = outp.pop_front() {
                    if !self.append_read(&bucket) {
                        return Attach::Broke;
                    }
                }
            }
        }
        Attach::On
    }

    /// `_php_stream_filter_flush` (`Z/main/streams/filter.c:360`): from the
    /// filter at `at` to the chain's tail, each called with nothing new,
    /// the first with `PSFS_FLAG_FLUSH_CLOSE` under `finish` and
    /// `PSFS_FLAG_FLUSH_INC` otherwise; a filter asking to be fed ends the
    /// flush, and one that breaks is `false`. What the last passes on goes
    /// into the read buffer for the read chain, after the bytes not yet
    /// read, and to the kind for the write chain:
    ///
    /// ```php
    /// class F extends php_user_filter {
    ///     function filter($in, $out, &$consumed, $closing): int {
    ///         while ($b = stream_bucket_make_writeable($in)) { $consumed += $b->datalen; stream_bucket_append($out, $b); }
    ///         if ($closing) stream_bucket_append($out, stream_bucket_new($this->stream, 'END'));
    ///         return PSFS_PASS_ON;
    ///     }
    /// }
    /// stream_filter_register('f', 'F');
    /// $m = fopen('php://memory', 'w+'); fwrite($m, 'abcdef'); rewind($m);
    /// $f = stream_filter_append($m, 'f', STREAM_FILTER_READ);
    /// var_dump(fread($m, 2), stream_filter_remove($f), stream_get_contents($m));
    /// ```
    /// ```expect
    /// string(2) "ab"
    /// bool(true)
    /// string(7) "cdefEND"
    /// ```
    fn flush_from(&mut self, chain: Chain, at: usize, finish: bool) -> bool {
        let mut inp = Brigade::new();
        let mut outp = Brigade::new();
        let mut flags = if finish {
            FilterFlags::FlushClose
        } else {
            FilterFlags::FlushInc
        };
        let mut current = at;
        while current < self.filters_on(chain) {
            let status = self.run_filter(chain, current, (&mut inp, &mut outp), None, flags);
            current += 1;
            match status {
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
                let Some((short, grown)) = self.with(|s| {
                    if s.readpos > 0 {
                        // The held bytes backed up to the front, and
                        // `writepos` moved back with them before `readpos`
                        // is zero, as 8.5.12 orders the two.
                        s.readbuf.copy_within(s.readpos..s.writepos, 0);
                        s.writepos -= s.readpos;
                        s.readpos = 0;
                    }
                    let grown = s.writepos + flushed + s.chunk_size();
                    (flushed > s.readbuf.len() - s.writepos, grown)
                }) else {
                    return false;
                };
                if short && !self.resize_readbuf(grown) {
                    return false;
                }
                while let Some(bucket) = inp.pop_front() {
                    if self.with(|s| s.push_read(&bucket)) != Some(true) {
                        return false;
                    }
                }
            }
            Chain::Write => {
                while let Some(bucket) = inp.pop_front() {
                    let _ = self.write_once(&bucket);
                }
            }
        }
        true
    }
}

impl PhpStream {
    /// `bucket` written after `writepos` into the room the buffer has;
    /// `false` where it has not the room.
    fn push_read(&mut self, bucket: &[u8]) -> bool {
        let Some(room) = self
            .readbuf
            .get_mut(self.writepos..self.writepos + bucket.len())
        else {
            return false;
        };
        room.copy_from_slice(bucket);
        self.writepos += bucket.len();
        true
    }
}

/// What [`StreamOp::attach`] did with a filter: put it on its chain, put
/// it there and then broke on the bytes the read buffer held, or refused
/// it the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attach {
    On,
    Broke,
    Refused,
}

/// The filter just attached to `chain`: its tail, `append`, or its head.
fn placed<F: ?Sized + OnChain>(
    p: &mut Pair<F>,
    chain: Chain,
    append: bool,
) -> Option<&mut Attached<F>> {
    let filters = p.chain(chain);
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
/// A filter out for its call is closed as it comes back.
fn release_filter(req: &mut Request, filter: Off) {
    let Off { ops, header, res } = filter;
    if let Some(res) = res {
        res.close(req);
        res.give_up(req);
    }
    if let Some(mut ops) = ops
        && let Err(thrown) = ops.as_filter_mut().close(req)
    {
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
    ) -> Option<Made>;
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
    ) -> Option<Made> {
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
            .code_mut()
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
                self.frames().active_function_name(),
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
        Some(Made { ops, header })
    }

    fn remove_filter(&mut self, res: Resource) -> Result<Option<bool>, Thrown> {
        if res.kind() != Some(self.types().filter) {
            return Ok(None);
        }
        let Some((stream, chain)) = res.native::<Named>(self).map(|n| (n.stream, n.chain)) else {
            return Ok(None);
        };
        let flushed = self
            .stream_op(stream, |op| {
                let at = op
                    .with(|s| pair!(s.filters.as_deref_mut()?, p => p.position(chain, res)))
                    .flatten()?;
                Some(op.flush_from(chain, at, true))
            })
            .flatten();
        if flushed == Some(false) {
            let text = format!(
                "{}(): Unable to flush filter, not removing",
                self.frames().active_function_name()
            );
            warning(self, &text);
            return Ok(Some(false));
        }
        let taken = self
            .stream_with(stream, |s| s.take_named(chain, res))
            .flatten();
        if let Some(filter) = taken {
            release_filter(self, filter);
        } else {
            res.close(self);
        }
        match self.diagnostics_mut().stream_thrown().take() {
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
        let persistent = req.stream_with(self, |s| s.flags.persistent) == Some(true);
        let Some(filter) = req.filter_create(name, None, persistent) else {
            return Ok(false);
        };
        let attached = req.stream_op(self, |op| op.attach(chain, filter, true));
        if attached == Some(Attach::Refused) {
            return match req.diagnostics_mut().stream_thrown().take() {
                Some(thrown) => Err(thrown),
                None => Ok(false),
            };
        }
        match req.diagnostics_mut().stream_thrown().take() {
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
                .stream_with(self, |s| s.mode.bytes().to_vec())
                .unwrap_or_default();
            if mode.contains(&b'r') || mode.contains(&b'+') {
                read_write |= FILTER_READ;
            }
            if mode.iter().any(|b| matches!(b, b'w' | b'+' | b'a')) {
                read_write |= FILTER_WRITE;
            }
        }
        let persistent = req.stream_with(self, |s| s.flags.persistent) == Some(true);
        let mut last = None;
        for (bit, chain) in [(FILTER_READ, Chain::Read), (FILTER_WRITE, Chain::Write)] {
            if read_write & bit == 0 {
                continue;
            }
            let Some(filter) = req.filter_create(name, params, persistent) else {
                return match req.diagnostics_mut().stream_thrown().take() {
                    Some(thrown) => Err(thrown),
                    None => Ok(None),
                };
            };
            let attached = req
                .stream_op(self, |op| op.attach(chain, filter, append))
                .unwrap_or(Attach::Broke);
            if attached != Attach::On {
                // `php_stream_filter_remove(filter, 1)`: the filter that
                // broke on the buffer taken off its chain and freed.
                let taken = (attached == Attach::Broke)
                    .then(|| {
                        req.stream_with(self, |s| {
                            pair!(s.filters.as_deref_mut()?, p => p.chain(chain).pop().map(Attached::off))
                        })
                    })
                    .flatten()
                    .flatten();
                if let Some(filter) = taken {
                    release_filter(req, filter);
                }
                return match req.diagnostics_mut().stream_thrown().take() {
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
            .stream_with(
                self,
                |s| pair!(s.filters.as_deref_mut()?, p => placed(p, chain, append)?.header.take()),
            )
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
        req.stream_with(self, |s| {
            if let Some(chains) = s.filters.as_deref_mut() {
                pair!(chains, p => {
                    if let Some(filter) = placed(p, chain, append) {
                        filter.res = held.take();
                    }
                });
            }
        });
        if let Some(unheld) = held {
            unheld.give_up(req);
        }
        // `RETURN_RES` with a throwable pending: the caller's owner is
        // freed with the return value, the chain's kept.
        if let Some(thrown) = req.diagnostics_mut().stream_thrown().take() {
            res.give_up(req);
            return Err(thrown);
        }
        Ok(Some(res))
    }
}

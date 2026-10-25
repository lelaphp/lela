//! How a stream is held (design §4.3 seam 5, #3415). php's `php_stream`
//! stays where its resource put it for the length of every operation on
//! it, so PHP code the operation serves, an error handler a warning of
//! the operation calls, a user filter, a user wrapper's method, a
//! notification callback or a crypto callback, reaches the live stream;
//! that code may `fclose()` it, which reads as closed while the
//! operation finishes on the stream it holds, and the stream is freed
//! as the operation ends.
//!
//! Here the stream is its resource's payload, a [`Slot`], and an
//! operation, an [`Op`], holds an owner of the resource and counts itself
//! in the slot's `holds` for its length, php's `GC_ADDREF` of what it
//! works on. It borrows the stream in segments ([`Op::with`]) through the
//! request, each a closure handed the stream and no request, so no
//! segment serves PHP code and the borrow checker holds every call into
//! PHP code outside one. A destructor that finds the stream held moves it
//! to the request's closed streams, where the operation still reaches it
//! and the last to end frees it. A part of the stream that is called with
//! the request, its kind above all ([`Op::kind`]), is lent out of the
//! stream for its call and put back after, so PHP code that reaches the
//! same stream meanwhile finds the stream there and the part in use,
//! php's `in_free` answer for an operation it cannot take.
//!
//! The one shared count beside the resource's is a socket's `Arc<Sock>`:
//! a crypto layer's call (`Op::with_crypto`) borrows the socket's fd for
//! its length while the socket stays in the stream and PHP code reaches
//! it, so the call holds its own clone of the socket, made as the layer
//! sets up, enables, connects or reads its meta data, and never per read
//! or write.
//!
//! ```php
//! $s = stream_socket_server('tcp://127.0.0.1:0');
//! $c = stream_socket_client('tcp://' . stream_socket_get_name($s, false), $e, $m, 0.2);
//! set_error_handler(function ($no, $text) use ($c) { echo implode(',', array_keys(stream_get_meta_data($c))), "\n"; var_dump(fwrite($c, "x")); fclose($c); return true; });
//! var_dump(stream_socket_enable_crypto($c, true, STREAM_CRYPTO_METHOD_TLS_CLIENT), is_resource($c));
//! ```
//! ```expect
//! timed_out,blocked,eof,stream_type,mode,unread_bytes,seekable,uri
//! int(1)
//! bool(false)
//! bool(false)
//! ```
//!
//! The same of a plain file's own notice, which the kind raises while it
//! is out for its read: the handler finds the stream, closes it, and the
//! read ends on the stream it holds, which is freed as it returns.
//!
//! ```php
//! $d = fopen(__DIR__, 'r');
//! set_error_handler(function ($no, $text) use (&$d) {
//!     echo $text, "\n";
//!     var_dump(stream_get_meta_data($d)['stream_type'], ftell($d), fclose($d));
//!     return true;
//! });
//! var_dump(fread($d, 10), is_resource($d));
//! ```
//! ```expect
//! fread(): Read of 8192 bytes failed with errno=21 Is a directory
//! string(5) "STDIO"
//! int(0)
//! bool(true)
//! bool(false)
//! bool(false)
//! ```

use super::{MetaData, Ops, PhpStream};
use crate::api::request::{Request, Stream};
use crate::api::values::{Owner, Resource};
use crate::state::Layer;

/// The payload of a `stream` or `persistent stream` resource, php's
/// `php_stream` behind `res->ptr`: the stream and how many operations
/// hold it, which its destructor takes.
pub(crate) struct Slot {
    pub(crate) stream: PhpStream,
    holds: u32,
}

/// What a resource of the two stream types holds as its native payload.
pub(crate) type Payload = Option<Slot>;

impl Slot {
    pub(crate) const fn new(stream: PhpStream) -> Slot {
        Slot { stream, holds: 0 }
    }
}

/// The slot of the stream `res` names: its payload while it is open, and
/// where an operation still holds it once closed, the request's closed
/// streams.
fn slot_mut(req: &mut Request, res: Resource) -> Option<&mut Slot> {
    if res.native::<Payload>(req).is_some_and(Option::is_some) {
        return res.native_mut::<Payload>(req)?.as_mut();
    }
    req.streams()
        .closed
        .iter_mut()
        .find(|(closed, _)| *closed == res)
        .map(|(_, slot)| slot)
}

/// The slot a destructor took off a closed resource: freed where nothing
/// holds it, and kept among the request's closed streams for the
/// operations that do, the last of which frees it ([`let_go`]).
pub(super) fn closed(req: &mut Request, res: Resource, slot: Slot) {
    if slot.holds == 0 {
        super::finish(req, slot.stream);
    } else {
        req.streams().closed.push((res, slot));
    }
}

/// One hold of the stream `res` names taken, with an owner of the
/// resource; `None` where it is closed or is not a stream's.
fn hold(req: &mut Request, res: Resource) -> Option<Owner<Resource>> {
    res.native_mut::<Payload>(req)?.as_mut()?.holds += 1;
    Some(Owner::take(req, res))
}

/// One hold given back: a stream closed meanwhile that nothing holds any
/// longer freed, as its destructor would have freed it, then the owner of
/// the resource given up, whose destructor, where it was the last, frees
/// the stream in turn.
fn let_go(req: &mut Request, res: Owner<Resource>) {
    if let Some(slot) = slot_mut(req, *res) {
        slot.holds = slot.holds.saturating_sub(1);
    }
    let closed = &mut req.streams().closed;
    if let Some(at) = closed
        .iter()
        .position(|(held, slot)| *held == *res && slot.holds == 0)
    {
        let (_, slot) = closed.remove(at);
        super::finish(req, slot.stream);
    }
    res.give_up(req);
}

/// Where an operation's stream is: in a resource it holds, or its own,
/// the stream no resource holds any longer, which its free runs over.
enum Place {
    Held(Owner<Resource>),
    Owned(PhpStream),
}

/// One operation on a stream: the request beside the stream, held for the
/// operation's length, so neither the stream nor the resource it reads as
/// goes while PHP code the operation serves lets go of them.
pub(crate) struct Op<'r> {
    pub(crate) req: &'r mut Request,
    place: Place,
}

impl<'r> Op<'r> {
    /// The operation on the stream `stream`'s resource holds, `None`
    /// where it is closed or is not a stream's.
    pub(super) fn begin(req: &'r mut Request, stream: Stream) -> Option<Op<'r>> {
        let res = hold(req, stream.resource())?;
        Some(Op {
            req,
            place: Place::Held(res),
        })
    }

    /// One segment: the stream borrowed for `f`, which is handed no
    /// request and so serves no PHP code. `None` only where no slot
    /// holds the stream any longer, as php's `in_free` answers.
    pub(crate) fn with<R>(&mut self, f: impl FnOnce(&mut PhpStream) -> R) -> Option<R> {
        match &mut self.place {
            Place::Held(res) => slot_mut(self.req, **res).map(|slot| f(&mut slot.stream)),
            Place::Owned(stream) => Some(f(stream)),
        }
    }

    /// The stream as PHP code reaches it, where a resource holds it.
    pub(crate) fn stream(&self) -> Option<Stream> {
        match &self.place {
            Place::Held(res) => Some(Stream::of(**res)),
            Place::Owned(_) => None,
        }
    }

    /// The stream's kind lent out of the stream for `f` and put back
    /// after, so `f` may serve PHP code; `None` for a kind in use by a
    /// call further out, which PHP code that call served is asking of.
    pub(crate) fn kind<R>(&mut self, f: impl FnOnce(&mut Request, &mut dyn Ops) -> R) -> Option<R> {
        let mut kind = self.with(|s| s.ops.lend())??;
        let out = f(self.req, kind.as_mut());
        self.with(|s| s.ops = Kind::In(kind));
        Some(out)
    }

    /// The operation's end: its hold given back ([`let_go`]).
    pub(super) fn end(self) {
        if let Place::Held(res) = self.place {
            let_go(self.req, res);
        }
    }
}

/// `f` over a stream no resource holds, as its free runs the generic
/// half's flush on it before it closes the kind: the stream back where
/// `f` is done with it, which no segment can move.
pub(super) fn owned<'r>(
    req: &'r mut Request,
    stream: PhpStream,
    f: impl FnOnce(&mut Op<'r>),
) -> Option<PhpStream> {
    let mut op = Op {
        req,
        place: Place::Owned(stream),
    };
    f(&mut op);
    match op.place {
        Place::Owned(stream) => Some(stream),
        Place::Held(_) => None,
    }
}

/// `f` with each of `streams` that is open held, as
/// [`StreamLayer::streams_held`](super::StreamLayer::streams_held) keeps
/// them for the length of a wait: each stream stays where it is, and one
/// closed meanwhile is freed as the wait returns.
pub(super) fn hold_each<R>(
    req: &mut Request,
    streams: &[Stream],
    f: impl FnOnce(&mut Request) -> R,
) -> R {
    let held: Vec<_> = streams
        .iter()
        .filter_map(|stream| hold(req, stream.resource()))
        .collect();
    let answered = f(req);
    for res in held {
        let_go(req, res);
    }
    answered
}

/// `stream->ops` with its `abstract`: the kind in the stream, or out of
/// it for a call that is called with the request ([`Op::kind`]), its
/// place answering for it meanwhile. Either way it reads as the kind's
/// operations, so a segment asks `s.ops` the kind's questions alike.
pub(crate) enum Kind {
    In(Box<dyn Ops>),
    Out(InUse),
}

impl Kind {
    /// The kind taken out, its place left answering what it answered
    /// as it went; `None` for one out already.
    fn lend(&mut self) -> Option<Box<dyn Ops>> {
        let Kind::In(kind) = self else {
            return None;
        };
        let place = InUse {
            label: kind.label(),
            seekable: kind.seekable(),
            meta: kind.meta_data(),
        };
        match std::mem::replace(self, Kind::Out(place)) {
            Kind::In(kind) => Some(kind),
            Kind::Out(_) => None,
        }
    }
}

impl std::ops::Deref for Kind {
    type Target = dyn Ops;

    fn deref(&self) -> &(dyn Ops + 'static) {
        match self {
            Kind::In(kind) => kind.as_ref(),
            Kind::Out(place) => place,
        }
    }
}

impl std::ops::DerefMut for Kind {
    fn deref_mut(&mut self) -> &mut (dyn Ops + 'static) {
        match self {
            Kind::In(kind) => kind.as_mut(),
            Kind::Out(place) => place,
        }
    }
}

/// The place of a kind out for a call: its label, whether it seeks and
/// its meta data as the call found them, and php's `-1` for every
/// operation PHP code the call serves asks of it.
pub(crate) struct InUse {
    label: &'static str,
    seekable: bool,
    meta: MetaData,
}

impl Ops for InUse {
    fn label(&self) -> &'static str {
        self.label
    }

    fn write(&mut self, _req: &mut Request, _buf: &[u8]) -> Option<usize> {
        None
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], _eof: &mut bool) -> Option<usize> {
        None
    }

    fn close(&mut self, _req: &mut Request) -> i32 {
        0
    }

    fn seekable(&self) -> bool {
        self.seekable
    }

    fn meta_data(&self) -> MetaData {
        self.meta
    }
}

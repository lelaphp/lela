//! Persistent streams (design §4.10, ledger 51, #1172): php's
//! `EG(persistent_list)` as far as `pfsockopen` and
//! `STREAM_CLIENT_PERSISTENT` reach it (`Z/main/streams/streams.c:68-138`,
//! `Z/main/streams/transports.c:75-93`), over the engine's [`Pool`].
//!
//! A request holds each entry it took or opened under php's key until it
//! ends, [`Held`]: in a resource of the `persistent stream` type, or idle
//! once that resource's last owner let go, which php's `NULL` regular-list
//! destructor leaves in the persistent list. A second ask by the key
//! answers the resource that holds it, one more owner, or a fresh resource
//! over the idle socket; `fclose` closes the socket for every holder and
//! drops the entry. At the request's end every entry it still holds is
//! given back to the pool, its resource and `stream->ctx` forgotten as
//! `forget_persistent_resource_id_numbers` forgets them; the next request
//! to ask the key takes it out, checked for liveness first. The pool is
//! the layer's engine state's ([`EngineState`]), typed by the entry it
//! keeps (#1962).
//!
//! [`Pool`]: crate::api::abi::Pool

use std::sync::Arc;

use crate::api::abi::Pool;
use crate::api::layer::Face as _;
use crate::state::EngineState;

use super::held::Payload;
use super::sockets::{Connection, Socket};
use super::{PhpStream, Request, Stream, StreamOp};
use crate::api::raising::Thrown;
use crate::api::request::{OptionAnswer, OwnedStream, StreamCtx, StreamOption};
use crate::api::values::{Owner, Resource};
use crate::state::Layer;
use crate::streams::StreamLayer;

/// One entry of the persistent list as the request holds it, under its
/// key: in the resource the request reaches it by, or idle, its resource
/// gone and the socket still open.
pub(crate) struct Held {
    key: Vec<u8>,
    state: State,
}

/// Where a held entry stands: the resource of the `persistent stream`
/// type whose payload it is, or the connection alone.
enum State {
    InResource(Resource),
    Idle(Idle),
}

/// An entry the request holds idle, its resource gone, and beside it what
/// of the request php's `php_stream` still holds then: `stream->ctx`, one
/// owner the stream's, which php keeps for the rest of the request and
/// forgets at its end (`forget_persistent_resource_id_numbers`,
/// `Z/main/streams/streams.c:68-87`). The next ask by the key in the same
/// request gives the stream it back ([`Persistent::persistent_stream`]);
/// the request's end lets it go ([`forget`]). The entry is engine state
/// and the `context` is the request's, so the two stand apart.
///
/// A stream asked for again by its key in the same request, with no
/// `context`, has the first open's, and its crypto layer reads it:
///
/// ```php
/// set_error_handler(function ($no, $msg) { echo $msg, "\n"; });
/// $s = stream_socket_server('tcp://127.0.0.1:0'); $addr = stream_socket_get_name($s, false);
/// $flags = STREAM_CLIENT_CONNECT | STREAM_CLIENT_PERSISTENT;
/// $a = stream_socket_client("tcp://$addr", $no, $str, 1, $flags, stream_context_create(['ssl' => ['local_cert' => '/nonexistent/cert.pem']]));
/// $peer = stream_socket_accept($s, 1);
/// $a = null;
/// $b = stream_socket_client("tcp://$addr", $no, $str, 1, $flags);
/// var_dump(stream_context_get_options($b));
/// fwrite($peer, "not a handshake\n"); fclose($peer);
/// var_dump(stream_socket_enable_crypto($b, true, STREAM_CRYPTO_METHOD_TLS_CLIENT));
/// ```
/// ```expect
/// array(1) {
///   ["ssl"]=>
///   array(1) {
///     ["local_cert"]=>
///     string(21) "/nonexistent/cert.pem"
///   }
/// }
/// stream_socket_enable_crypto(): Unable to set local cert chain file `/nonexistent/cert.pem'; Check that your cafile/capath settings include details of your certificate and its issuer
/// bool(false)
/// ```
struct Idle {
    entry: Entry,
    context: Option<Owner<StreamCtx>>,
}

/// What the persistent list keeps of a stream across its resources and
/// across requests: the stream bare of its filters ([`Bare`]), and the
/// filters of its chains, which stay on the stream as php's `php_stream`
/// keeps them, their resources the request's and gone with it. Nothing
/// of a request's.
pub(crate) struct Entry {
    bare: Bare,
    filters: Kept,
}

/// A persistent stream bare of its filters: the socket's [`Connection`],
/// with its `transport`'s and crypto layer's state, and
/// `stream->orig_path`, which php allocates persistent beside it. What
/// [`keep`] takes out for the extension that opened the stream, as
/// [`Persisted`], which holds no filter. Engine state by type (#4541):
/// no field of it can hold a request's value.
///
/// [`Persisted`]: crate::api::request::Persisted
#[derive(php_engine::api::abi::EngineState)]
pub(crate) struct Bare {
    socket: Connection,
    orig_path: Option<Box<[u8]>>,
}

pub(crate) trait Persistent {
    fn persistent_stream(&mut self, key: &[u8]) -> Result<Option<Owner<Stream>>, Thrown>;
    fn lend(&mut self, key: Vec<u8>, entry: Entry) -> Result<Owner<Stream>, Thrown>;
    fn hold_persistent(&mut self, key: Vec<u8>, stream: Stream);
    fn drop_persistent(&mut self, res: Resource);
    fn persistent_pool(&mut self) -> Option<Arc<EngineState>>;
}

impl Persistent for Request {
    /// `php_stream_from_persistent_id` and the liveness check
    /// `php_stream_xport_create` makes of what it found (`transports.c:75-93`):
    /// the stream the request holds under `key`, its resource answered with
    /// one more owner, or a fresh `persistent stream` resource over the
    /// socket held idle or taken out of the engine's pool, where
    /// `PHP_STREAM_OPTION_CHECK_LIVENESS` finds it alive; a dead one is
    /// `php_stream_pclose`d, and `None` has the caller open a new one.
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0');
    /// [$host, $port] = explode(':', stream_socket_get_name($s, false));
    /// $a = pfsockopen($host, $port);
    /// $peer = stream_socket_accept($s, 1);
    /// fwrite($a, "one");
    /// $a = null;
    /// $b = pfsockopen($host, $port);
    /// fwrite($b, "two"); usleep(50000);
    /// var_dump(get_resource_type($b), fread($peer, 10), @stream_socket_accept($s, 0.2));
    /// fclose($peer); usleep(100000);
    /// var_dump(get_resource_type(pfsockopen($host, $port)), is_resource(@stream_socket_accept($s, 1)));
    /// ```
    /// ```expect
    /// string(17) "persistent stream"
    /// string(6) "onetwo"
    /// bool(false)
    /// string(17) "persistent stream"
    /// bool(true)
    /// ```
    fn persistent_stream(&mut self, key: &[u8]) -> Result<Option<Owner<Stream>>, Thrown> {
        let stream = if let Some(at) = self
            .streams()
            .persistent
            .iter()
            .position(|held| held.key == key)
        {
            let Held { key, state } = self.streams().persistent.remove(at);
            match state {
                State::InResource(res) => {
                    let state = State::InResource(res);
                    self.streams().persistent.insert(at, Held { key, state });
                    Owner::take(self, Stream::of(res))
                }
                State::Idle(Idle { entry, context }) => {
                    let stream = self.lend(key, entry);
                    // The `context` the stream kept goes back to it, its
                    // socket's crypto layer reading it as before.
                    if let (Ok(stream), Some(context)) = (&stream, &context) {
                        self.set_stream_context(**stream, Some(**context));
                    }
                    if let Some(context) = context {
                        context.give_up(self);
                    }
                    stream?
                }
            }
        } else if let Some(entry) = self
            .persistent_pool()
            .and_then(|engine| engine.persistent.take(&key.to_vec()))
        {
            self.lend(key.to_vec(), entry)?
        } else {
            return Ok(None);
        };
        let alive = self
            .stream_op(*stream, |op| op.set_option(StreamOption::Liveness))
            .is_some_and(|answer| answer == OptionAnswer::Ok);
        if alive {
            return Ok(Some(stream));
        }
        // `php_stream_pclose`: the socket closed and the entry dropped,
        // the resource closed and the owner this ask took given up.
        PhpStream::close_open(self, *stream);
        stream.give_up(self);
        Ok(None)
    }

    /// `entry` under a fresh resource of the `persistent stream` type,
    /// held under `key`: the stream `php_stream_from_persistent_id`
    /// registers over what the list kept, `r+`, and seeking nowhere.
    ///
    /// A persistent stream's `php_stream`, its socket's data and its read
    /// buffer are `pemalloc`'d persistent, so a request counts its
    /// resource alone:
    ///
    /// ```php
    /// $s = stream_socket_server('tcp://127.0.0.1:0'); [$host, $port] = explode(':', stream_socket_get_name($s, false));
    /// $m = memory_get_usage(); $p = pfsockopen($host, $port); echo memory_get_usage() - $m, "\n";
    /// $a = stream_socket_accept($s); fwrite($a, "hi\n"); usleep(20000);
    /// $m = memory_get_usage(); $l = fgets($p); echo memory_get_usage() - $m, ' ', $l;
    /// ```
    /// ```expect
    /// 32
    /// 40 hi
    /// ```
    fn lend(&mut self, key: Vec<u8>, entry: Entry) -> Result<Owner<Stream>, Thrown> {
        let stream = reopen(self, entry)?;
        self.persistent_pool();
        self.streams().persistent.push(Held {
            key,
            state: State::InResource(stream.resource()),
        });
        Ok(stream)
    }

    /// A stream `php_stream_xport_create` has just opened under `key`,
    /// now held as the request's: its resource's type is `persistent
    /// stream`, which [`Request::stream_alloc_as`] gave it.
    fn hold_persistent(&mut self, key: Vec<u8>, stream: Stream) {
        self.persistent_pool();
        self.streams().persistent.push(Held {
            key,
            state: State::InResource(stream.resource()),
        });
    }

    /// `_php_stream_free`'s `PHP_STREAM_FREE_PERSISTENT` arm, what
    /// `fclose` reaches for a persistent stream: the entry the resource
    /// held dropped, its socket closed already.
    fn drop_persistent(&mut self, res: Resource) {
        self.streams()
            .persistent
            .retain(|held| !matches!(held.state, State::InResource(r) if r == res));
    }

    /// The layer's engine state, whose pool is `EG(persistent_list)`,
    /// kept in the request's state once asked for, so the entries held go
    /// back to it as the request is dropped ([`give_back`]); `None` on an
    /// engine no stream layer was supplied to, where a held entry closes
    /// with the request.
    fn persistent_pool(&mut self) -> Option<Arc<EngineState>> {
        if let Some(engine) = &self.streams().engine {
            return Some(Arc::clone(engine));
        }
        let engine = self.layer_state::<EngineState>()?;
        self.streams().engine = Some(Arc::clone(&engine));
        Some(engine)
    }
}

/// `entry` under a fresh resource of the `persistent stream` type, `r+`
/// and seeking nowhere, held under no key.
fn reopen(req: &mut Request, entry: Entry) -> Result<Owner<Stream>, Thrown> {
    let Bare { socket, orig_path } = entry.bare;
    let mut stream = PhpStream::new(Box::new(Socket::from(socket)), b"r+");
    stream.orig_path = orig_path.map(super::OrigPath::Persistent);
    stream.flags.no_seek = true;
    stream.attach_kept(entry.filters);
    req.stream_alloc_as(stream, true)
}

/// What [`Streams::stream_lend`](crate::api::request::Streams::stream_lend)
/// lends: the connection [`keep`] took out, under a fresh resource taken
/// off both of the request's lists at once ([`unlist`]), as php's
/// persistent mysqlnd connection carries its `php_stream` from request to
/// request in no list at all; with no filters, as it was kept.
pub(crate) fn lend_alone(req: &mut Request, bare: Bare) -> Result<Owner<Stream>, Thrown> {
    let entry = Entry {
        bare,
        filters: Kept::default(),
    };
    let stream = reopen(req, entry)?;
    unlist(req, *stream);
    Ok(stream)
}

/// What php's mysqlnd does to the stream it opens
/// (`mysqlnd_vio::open_tcp_or_unix`, `Z/ext/mysqlnd/mysqlnd_vio.c:139-175`):
/// a persistent stream deleted from `EG(persistent_list)` without its
/// destructor, and every stream from `EG(regular_list)`
/// (`mysqlnd_fixup_regular_list`), so the stream is its owner's alone,
/// one the request's end neither closes nor holds idle under its key.
pub(crate) fn unlist(req: &mut Request, stream: Stream) {
    let res = stream.resource();
    req.drop_persistent(res);
    res.unlist(req);
}

/// `ZEND_MODULE_POST_ZEND_DEACTIVATE_D(streams)` at 8.5.12, which was
/// `PHP_RSHUTDOWN_FUNCTION(streams)` before, and the request's end
/// (`forget_persistent_resource_id_numbers`, `streams.c:68-101`): every
/// entry the request still holds given back to the engine's pool, its
/// resource and its `stream->ctx` forgotten, nothing of them released,
/// since the request's resources, each `stream->ctx` among them, are
/// destroyed already, and an idle entry holds neither by type
/// ([`Connection`]). Served as the request's stream state is dropped,
/// after the resources are closed, when every entry is idle.
pub(crate) fn give_back(pool: &Pool<Vec<u8>, Entry>, held: Vec<Held>) {
    for held in held {
        // `stream->ctx` was let go at the request's post-deactivation
        // ([`forget`]), which every request that ends goes through; none
        // is left here to give up.
        if let State::Idle(Idle { entry, context: _ }) = held.state {
            // An entry past the pool's bound is closed here, outside the
            // lock.
            drop(pool.give_back(held.key, entry));
        }
    }
}

/// The socket of a persistent stream taken out for the extension that
/// opened it to keep across requests, as php's persistent mysqlnd
/// connection keeps its `php_stream`: its owed write flushed first, out
/// of the request's persistent list, where an unlisted stream is not
/// already, and out of the resource, which is closed, and the caller's
/// owner given up: the one place a socket becomes what outlives every
/// request, its connection taken out whole and what of the request it
/// held left to close with the stream ([`Socket::detach`]), and its
/// filters, none of which travel. `None`, and the stream freed, for a
/// stream that is not a socket's.
pub(crate) fn keep(req: &mut Request, stream: Owner<Stream>) -> Option<Bare> {
    let res = stream.resource();
    let socket = req.stream_with(*stream, |s| s.ops.as_socket().is_some());
    if socket != Some(true) {
        stream.free(req);
        return None;
    }
    req.stream_op(*stream, StreamOp::flush_owed);
    let taken = res.native_mut::<Payload>(req).and_then(Option::take);
    res.close(req);
    req.drop_persistent(res);
    // A stream an operation holds is that operation's to free.
    let taken = match taken {
        Some(slot) if slot.is_held() => {
            super::held::release_slot(req, res, slot);
            None
        }
        taken => taken,
    };
    let kept = taken.and_then(|slot| {
        let mut taken = slot.stream;
        let socket = taken.ops.as_socket()?.detach();
        let orig_path = taken.orig_path.take().map(|path| path.into_persistent(req));
        req.streams().pclose_ret = taken.release(req);
        Some(Bare { socket, orig_path })
    });
    stream.give_up(req);
    kept
}

/// `stream_resource_persistent_dtor`'s regular-list half, php's `NULL`
/// (`Z/main/streams/streams.c:1911`): a `persistent stream` resource whose
/// last owner let go, or which the request's shutdown closed, leaves its
/// socket open and held idle under its key. One held under none, which
/// its owner took off the lists ([`unlist`]), is closed, as
/// `php_stream_free` closes the stream it reaches with no resource. The
/// held one keeps its filters, so the next ask by the key writes through
/// them:
///
/// ```php
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// [$host, $port] = explode(':', stream_socket_get_name($s, false));
/// $a = pfsockopen($host, $port);
/// $peer = stream_socket_accept($s, 1);
/// stream_filter_append($a, 'string.toupper', STREAM_FILTER_WRITE);
/// $a = null;
/// $b = pfsockopen($host, $port);
/// fwrite($b, "abc"); usleep(50000);
/// var_dump(fread($peer, 10));
/// ```
/// ```expect
/// string(3) "ABC"
/// ```
pub(crate) fn dtor(req: &mut Request, r: Resource) -> Result<(), Thrown> {
    super::dtor(req, r)
}

/// [`dtor`]'s work on the stream once nothing holds it: a stream held
/// under its key keeps its socket idle there, and every other is freed;
/// what the kind's close answered.
pub(crate) fn finish(req: &mut Request, stream: PhpStream) -> i32 {
    // Where the entry stands is asked each time it is used: closing the
    // filters' resources and flushing the write chain may serve PHP code
    // that changes the list.
    let held_at = |req: &mut Request, stream: &PhpStream| {
        let r = stream.res?;
        req.streams()
            .persistent
            .iter()
            .position(|held| matches!(held.state, State::InResource(res) if res == r))
    };
    let mut stream = stream;
    if held_at(req, &stream).is_some() && stream.ops.as_socket().is_some() {
        // php's destructor is `NULL`: nothing is flushed and the filters
        // stay on the stream, to end with it at the engine's end
        // ([`engine_end`]) or travel with it to the request that asks
        // the key next.
        let filters = stream.keep_filters(req);
        // `stream->ctx` stays the stream's for the rest of the request,
        // beside the entry and not in it.
        let context = stream.wrapped.as_mut().and_then(|w| w.context.take());
        let entry = stream.ops.as_socket().map(|socket| Entry {
            bare: Bare {
                socket: socket.detach(),
                orig_path: stream
                    .orig_path
                    .take()
                    .map(|path| path.into_persistent(req)),
            },
            filters,
        });
        match (entry, held_at(req, &stream)) {
            (Some(entry), Some(at)) => {
                req.streams().persistent[at].state = State::Idle(Idle { entry, context });
            }
            (entry, _) => {
                drop(entry);
                if let Some(context) = context {
                    context.give_up(req);
                }
            }
        }
    } else {
        super::held::owned(req, &mut stream, StreamOp::flush_owed);
        if let Some(at) = held_at(req, &stream) {
            req.streams().persistent.remove(at);
        }
    }
    stream.release(req)
}

/// `ZEND_MODULE_POST_ZEND_DEACTIVATE_D(streams)`, which `basic_functions`
/// registers (`Z/main/streams/streams.c:90-98`), and its
/// `forget_persistent_resource_id_numbers`: each entry the request holds
/// idle lets go of `stream->ctx`, after the request's resources, that
/// `context` among them, are closed. The entries stay held, to go back to
/// the engine's pool as the request's stream state is dropped
/// ([`give_back`]).
pub(crate) fn forget(req: &mut Request) {
    // A request that never touched a stream holds none, and its stream
    // state is not built for the asking.
    if req
        .streams_ref()
        .is_none_or(|state| state.persistent.is_empty())
    {
        return;
    }
    let contexts: Vec<Owner<StreamCtx>> = (req.streams().persistent.iter_mut())
        .filter_map(|held| match &mut held.state {
            State::Idle(idle) => idle.context.take(),
            State::InResource(_) => None,
        })
        .collect();
    for context in contexts {
        context.give_up(req);
    }
}

/// `zend_destroy_rsrc_list(&EG(persistent_list))`'s free of an entry
/// that kept filters: the stream opened again in the engine's end and
/// freed, `php_stream_free(PHP_STREAM_FREE_CLOSE)`, so its write chain
/// is flushed closing and each filter's `dtor` served where php serves
/// them, labelled `PHP Shutdown`. An entry with none is closed as it is
/// dropped. What it raises is written past the request, so the CLI's
/// own test holds it (`sapi/cli/tests/persistent_filters.rs`).
pub(crate) fn end_entry(req: &mut Request, entry: Entry) {
    if entry.filters.is_empty() {
        return;
    }
    if let Ok(stream) = reopen(req, entry) {
        PhpStream::close_open(req, *stream);
        stream.give_up(req);
    }
}

/// The filters a persistent stream keeps between its resources and
/// across requests: each chain's `fops`, in order, with no resource and
/// on no heap of a request's.
#[derive(Default)]
pub(crate) struct Kept {
    pub(super) read: Vec<Box<dyn crate::api::request::StreamFilter>>,
    pub(super) write: Vec<Box<dyn crate::api::request::StreamFilter>>,
}

impl Kept {
    fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty()
    }
}

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
//! to ask the key takes it out, checked for liveness first.
//!
//! [`Pool`]: crate::api::abi::Pool

use std::sync::Arc;

use crate::api::abi::PersistentList;

use super::sockets::Socket;
use super::{PhpStream, Request, Stream};
use crate::api::raising::Thrown;
use crate::api::request::{OptionAnswer, StreamOption};
use crate::api::values::Resource;
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
    Idle(Entry),
}

/// What the persistent list keeps of a stream across its resources and
/// across requests: the socket, with its `transport`'s and crypto layer's
/// state, and `stream->orig_path`, which php allocates persistent beside
/// it. Nothing of a request's.
pub(crate) struct Entry {
    socket: Socket,
    orig_path: Option<Box<[u8]>>,
}

pub(crate) trait Persistent {
    fn persistent_stream(&mut self, key: &[u8]) -> Result<Option<Stream>, Thrown>;
    fn lend(&mut self, key: Vec<u8>, entry: Entry) -> Result<Stream, Thrown>;
    fn hold_persistent(&mut self, key: Vec<u8>, stream: Stream);
    fn drop_persistent(&mut self, res: Resource);
    fn persistent_pool(&mut self) -> Arc<PersistentList>;
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
    fn persistent_stream(&mut self, key: &[u8]) -> Result<Option<Stream>, Thrown> {
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
                    self.addref(&res.as_value());
                    Stream::of(res)
                }
                State::Idle(entry) => self.lend(key, entry)?,
            }
        } else if let Some(entry) = self.persistent_pool().take(&key.to_vec()) {
            let Ok(entry) = entry.downcast::<Entry>() else {
                return Ok(None);
            };
            self.lend(key.to_vec(), *entry)?
        } else {
            return Ok(None);
        };
        let alive = self
            .stream_op(stream, |req, s| s.set_option(req, StreamOption::Liveness))
            .is_some_and(|answer| answer == OptionAnswer::Ok);
        if alive {
            return Ok(Some(stream));
        }
        // `php_stream_pclose`: the socket closed and the entry dropped,
        // the resource closed and the owner this ask took given up.
        PhpStream::close_open(self, stream);
        self.release(stream.as_value());
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
    /// 32 hi
    /// ```
    fn lend(&mut self, key: Vec<u8>, entry: Entry) -> Result<Stream, Thrown> {
        let mut stream = PhpStream::new(Box::new(entry.socket), b"r+");
        stream.orig_path = entry.orig_path.map(super::OrigPath::Persistent);
        stream.flags.no_seek = true;
        let stream = self.stream_alloc_as(stream, true)?;
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

    /// The engine's pool, `EG(persistent_list)`, kept in the request's
    /// state once asked for, so the entries held go back to it as the
    /// request is dropped ([`give_back`]).
    fn persistent_pool(&mut self) -> Arc<PersistentList> {
        if let Some(pool) = &self.streams().pool {
            return Arc::clone(pool);
        }
        let pool = self.persistent_list();
        self.streams().pool = Some(Arc::clone(&pool));
        pool
    }
}

/// `PHP_RSHUTDOWN_FUNCTION(streams)` and the request's end
/// (`forget_persistent_resource_id_numbers`, `streams.c:68-101`): every
/// entry the request still holds given back to the engine's pool, its
/// resource and its `stream->ctx` forgotten. Served as the request's
/// stream state is dropped, after the resources are closed, when every
/// entry is idle.
pub(crate) fn give_back(pool: &PersistentList, held: Vec<Held>) {
    for held in held {
        if let State::Idle(mut entry) = held.state {
            entry.socket.forget_request();
            // An entry past the pool's bound is closed here, outside the
            // lock.
            drop(pool.give_back(held.key, Box::new(entry)));
        }
    }
}

/// What php's mysqlnd does with the persistent stream it opens
/// (`mysqlnd_vio::open_tcp_or_unix`, `Z/ext/mysqlnd/mysqlnd_vio.c:139-169`),
/// deleting it from `EG(persistent_list)` and from `EG(regular_list)` so
/// that the connection is its owner's alone: the socket of `stream` taken
/// out, its owed write flushed first, out of the request's persistent list
/// and out of the resource, which is closed, and the caller's owner given
/// up. `None`, and the stream left as it is, for a stream that is not a
/// socket's.
pub(crate) fn keep(req: &mut Request, stream: Stream) -> Option<Entry> {
    let res = stream.resource();
    let socket = req.stream_op(stream, |_, s| s.ops.as_socket().is_some());
    if socket != Some(true) {
        return None;
    }
    req.stream_op(stream, |req, s| s.flush_owed(req));
    let taken = res
        .native_mut::<Option<PhpStream>>(req)
        .and_then(Option::take);
    res.close(req);
    req.drop_persistent(res);
    let kept = taken.and_then(|mut taken| {
        let mut socket = taken.ops.as_socket()?.detach();
        socket.forget_request();
        let orig_path = taken.orig_path.take().map(|path| path.into_persistent(req));
        req.streams().pclose_ret = taken.release(req);
        Some(Entry { socket, orig_path })
    });
    req.release(stream.as_value());
    kept
}

/// `stream_resource_persistent_dtor`'s regular-list half, php's `NULL`
/// (`Z/main/streams/streams.c:1911`): a `persistent stream` resource whose
/// last owner let go, or which the request's shutdown closed, leaves its
/// socket open and held idle under its key.
pub(crate) fn dtor(req: &mut Request, r: Resource) {
    let Some(mut stream) = r
        .native_mut::<Option<PhpStream>>(req)
        .and_then(Option::take)
    else {
        return;
    };
    stream.flush_owed(req);
    let entry = stream.ops.as_socket().map(|socket| Entry {
        socket: socket.detach(),
        orig_path: stream
            .orig_path
            .take()
            .map(|path| path.into_persistent(req)),
    });
    let Some(at) = req
        .streams()
        .persistent
        .iter()
        .position(|held| matches!(held.state, State::InResource(res) if res == r))
    else {
        return;
    };
    match entry {
        Some(entry) => req.streams().persistent[at].state = State::Idle(entry),
        None => {
            req.streams().persistent.remove(at);
        }
    }
    req.streams().pclose_ret = stream.release(req);
}

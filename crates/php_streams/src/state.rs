//! The stream layer's request state (design §4.3 seam 5, ADR-0005): what
//! php keeps for streams in `FG()` and `BG()`, the wrapper and filter
//! tables, the stat cache and the persistent entries a request holds,
//! reached as `req.state::<State>()`, built on the request's first touch
//! and dropped with it.

use std::sync::Arc;

use crate::api::abi::PersistentList;
use crate::api::layer::Face as _;
use php_engine::api::values::ResourceType;

use crate::api::request::{
    EngineWrapper, FilterFactory, Request, Standard, StreamCtx, StreamWrapper, context_dtor,
};
use crate::api::values::{Owner, Resource};
use crate::streams::persistent::Held;
use crate::streams::{FilterCall, Registered};
use crate::wrapper::StatCache;

/// One request's streams.
pub(crate) struct State {
    /// The resource types the layer registered as the state was built.
    pub(crate) types: Option<Types>,
    /// The operations of each wrapper an extension registered, which a
    /// [`crate::streams::Located::Ext`] row names by its place.
    pub(crate) wrappers: Vec<StreamWrapper>,
    /// `url_stream_wrappers_hash`: the table every request starts with,
    /// the layer's own wrappers and then the extensions' in the order
    /// they registered them, which is php's startup order since the
    /// extensions activate in it; the layer's three stand first until
    /// the extension that registers them moves them to its place.
    pub(crate) url_stream_wrappers: Vec<Registered>,
    /// `FG(stream_wrappers)`: the request's own wrapper table once a
    /// program changed it, `None` while it has the built-in one.
    pub(crate) url_wrappers: Option<Vec<Registered>>,
    /// `FG(user_stream_current_filename)`: the path a user-space wrapper's
    /// opener is opening, which the same path opened from inside it is
    /// refused as.
    pub(crate) user_stream_current: Option<Vec<u8>>,
    /// The `php_stream_context` the stream operation in progress was
    /// handed (`Streams::with_stream_ctx`), which a user-space wrapper's
    /// instance is given as its `$context`; `None` is php's `NULL`.
    pub(crate) stream_ctx: Option<StreamCtx>,
    /// `FG(default_context)`, made on its first use
    /// ([`StreamCtx::default_ctx`]); where php keeps it is not
    /// observable, so the layer keeps it beside the `context` (#1777).
    pub(crate) default_context: Option<Owner<StreamCtx>>,
    /// `PG(in_user_include)`: a user-space wrapper that is no URL wrapper
    /// is opening for an include under `allow_url_include=0`, so the
    /// opens it makes are held to `allow_url_include` as an include's are.
    pub(crate) in_user_include: bool,
    /// `FG(pclose_ret)`: what the last stream freed by its resource's
    /// destructor answered from its kind's close, which `pclose()` reads.
    pub(crate) pclose_ret: i32,
    /// `FG(stream_filters)`: each filter name and its factory in the
    /// order they were registered, the extensions' in php's startup
    /// order and a program's after.
    pub(crate) filters: Vec<(Vec<u8>, FilterFactory)>,
    /// The streams an operation holds while it serves PHP code on them,
    /// php's `PHP_STREAM_FLAG_NO_FCLOSE`, which `fclose` refuses.
    pub(crate) held_streams: Vec<Resource>,
    /// The streams `STDIN`, `STDOUT` and `STDERR` were registered on,
    /// each until it is closed.
    pub(crate) standard_streams: [Option<Resource>; 3],
    /// Which of the three constants' streams a program closed.
    pub(crate) standard_closed: [bool; 3],
    /// `FG(wrapper_errors)` of the extension wrapper whose opener is
    /// running: what `Streams::wrapper_log_error` logged, which
    /// `php_stream_display_wrapper_errors` prints where the open fails.
    pub(crate) wrapper_errors: Vec<String>,
    /// The `*opened_path` an extension wrapper's opener set for the open
    /// being served (`Streams::set_opened_path`), which an include
    /// compiles the file under.
    pub(crate) opened_path: Option<Vec<u8>>,
    /// The filter call in progress, whose stream is out of its resource
    /// while the filter runs.
    pub(crate) filtering: Option<FilterCall>,
    /// `BG(CurrentStatFile)` and `BG(CurrentLStatFile)`, `php_stat`'s
    /// cache, which the plain files kind clears.
    pub(crate) stat_cache: StatCache,
    /// The entries of php's `EG(persistent_list)` this request holds,
    /// lent from the engine's pool or opened here, until it ends
    /// ([`crate::streams::persistent`]).
    pub(crate) persistent: Vec<Held>,
    /// The engine's pool, once the request took or held an entry: what
    /// the entries still held go back to as the request is dropped.
    pub(crate) pool: Option<Arc<PersistentList>>,
}

/// The four resource types `PHP_MINIT(streams)`, `PHP_MINIT(file)` and
/// `PHP_MINIT(user_filters)` register: `le_stream`, `le_pstream`,
/// `le_stream_context` and `le_stream_filter`.
#[derive(Clone, Copy)]
pub(crate) struct Types {
    pub(crate) stream: ResourceType,
    pub(crate) pstream: ResourceType,
    pub(crate) context: ResourceType,
    pub(crate) filter: ResourceType,
}

impl Default for State {
    fn default() -> State {
        State {
            types: None,
            wrappers: Vec::new(),
            url_stream_wrappers: layer_wrappers(),
            url_wrappers: None,
            user_stream_current: None,
            stream_ctx: None,
            default_context: None,
            in_user_include: false,
            pclose_ret: 0,
            filters: Vec::new(),
            held_streams: Vec::new(),
            standard_streams: [None; 3],
            standard_closed: [false; 3],
            wrapper_errors: Vec::new(),
            opened_path: None,
            filtering: None,
            stat_cache: StatCache::default(),
            persistent: Vec::new(),
            pool: None,
        }
    }
}

impl Drop for State {
    /// The rest of `php_request_shutdown` for the persistent list: every
    /// entry the request still holds idle given back to the engine's
    /// pool, after the resources that held them closed.
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            crate::streams::persistent::give_back(&pool, std::mem::take(&mut self.persistent));
        }
    }
}

/// The request's stream state, built on its first touch.
pub(crate) trait Layer {
    /// The state, mutably.
    fn streams(&mut self) -> &mut State;
    /// The state, read; `None` before the request touched a stream.
    fn streams_ref(&self) -> Option<&State>;
    /// The three resource types, registered as the state is built.
    fn types(&mut self) -> Types;
    /// `fclose()` reaching one of the constants' streams: the host's fd
    /// under it closed, and the constant's stream forgotten.
    fn close_standard(&mut self, closed: Resource);
    /// Whether the constant over one of the three was closed.
    fn standard_closed(&self, standard: Standard) -> bool;
    /// Whether `php://stdout` and `php://stderr` are copies of the
    /// process's fds, as under php's CLI once the constants stand.
    fn copies_standard(&self) -> bool;
}

impl Layer for Request {
    /// The state is built with the three resource types registered, as
    /// `php_module_startup` registers them before any request, so
    /// `get_resources('stream')` names a type once the extension that
    /// registers the plain files wrapper has started.
    fn streams(&mut self) -> &mut State {
        if self.state_ref::<State>().is_none() {
            let types = register_types(self);
            self.state::<State>().types = Some(types);
        }
        self.state::<State>()
    }

    fn streams_ref(&self) -> Option<&State> {
        self.state_ref::<State>()
    }

    fn types(&mut self) -> Types {
        if let Some(types) = self.streams().types {
            return types;
        }
        let types = register_types(self);
        self.streams().types = Some(types);
        types
    }

    fn close_standard(&mut self, closed: Resource) {
        let state = self.streams();
        let Some(at) = state
            .standard_streams
            .iter()
            .position(|s| *s == Some(closed))
        else {
            return;
        };
        state.standard_streams[at] = None;
        state.standard_closed[at] = true;
        let standard = [Standard::Stdin, Standard::Stdout, Standard::Stderr][at];
        self.close_host_fd(standard);
    }

    fn standard_closed(&self, standard: Standard) -> bool {
        self.streams_ref()
            .is_some_and(|state| state.standard_closed[standard as usize])
    }

    fn copies_standard(&self) -> bool {
        self.streams_ref()
            .is_some_and(|state| state.standard_streams.iter().any(Option::is_some))
            && self.host_fds_are_process()
    }
}

/// The layer's resource types, registered as its state is built.
fn register_types(req: &mut Request) -> Types {
    let (stream, pstream) = crate::streams::register(req.resources());
    let context = req
        .resources()
        .register_type(b"stream-context", Some(context_dtor));
    let filter = req.resources().register_type(b"stream filter", None);
    Types {
        stream,
        pstream,
        context,
        filter,
    }
}

/// The layer's own wrappers, `php`, `file` and `data`: the table every
/// request starts from until `standard`, which registers them, moves
/// them to its place.
pub(crate) fn layer_wrappers() -> Vec<Registered> {
    [
        EngineWrapper::Php,
        EngineWrapper::Plain,
        EngineWrapper::Data,
    ]
    .map(Registered::engine)
    .into()
}

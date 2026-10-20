//! The ABI group (design §4.4, §7.1): the [`Extension`] value and its
//! builder, the marshalling vocabulary a native's signature is written
//! in, and the two state declarations of ADR-0005 with [`Cache`], the
//! one thing that changes across requests. The marshalling traits and
//! the class traits the native attributes expand against are the two
//! submodules', re-exported here.

use std::any::{Any, TypeId};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use php_heap::{Fatal, Heap, Obj};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ConstDecl, FunctionDecl, IniDecl, MethodDecl, Name, PropertyDecl,
    Visibility,
};

use crate::api::call::{Function, Target};
use crate::api::coerce::BinaryOp;
use crate::api::raising::{Level, Thrown};
use crate::api::request::{HookedTable, Request};
use crate::api::values::{
    ClassId, ClosureInfo, Owner, PhpArray, PhpObject, PhpRef, PhpString, Place, TypeTag, Value,
    walk,
};
use crate::supplied::{Keyed, Registered, Supplies};

mod class;
mod marshal;

pub use crate::fiber::{Coroutine, CoroutineEnd, CoroutineStatus, HoldsCoroutine};
pub use crate::observer::{
    BeginHandler, EndHandler, ErrorNotice, FiberHandler, FiberStatus, FiberSwitchHandler, Observed,
    ObservedFiber, ObserverHandlers, ObserverInit, ObserverRegistered, OpcodeHandler, Opcodes,
    OpcodesDeclared, Removed,
};
use crate::observer::{ClassLinked, ErrorObserver, FunctionDeclared};
pub use crate::statement::{Statement, StatementHandler};
pub use crate::supplied::{Beneath, ChainKind, Compiled};

/// The chained kinds (#1945), each a marker [`Beneath`] is generic
/// over: php's saved pointer of the name, entered from the top with each
/// link handed the ones beneath.
pub mod chain {
    pub use crate::supplied::chain::{
        CompileFile, CompileString, Error, GcCollect, Interrupt, Opcode, PostStartup, ResolvePath,
        Throw, Upload,
    };
}
pub use class::{
    Cases, Class, ClassRef, ConstArray, ConstValue, Header, IntoConst, Members, PropertyValue,
    Typed,
};
pub use marshal::{
    Absent, False, FromArgument, Instance, IntoReturn, Iterable, NamedVariadic, Never, Null,
    NullDefault, OrFalse, Passed, Path, Raw, Ref, Static, This, True, Union, Untyped, Variadic,
};
/// The encoding functions' vocabulary (`zend_multibyte_functions`),
/// which the compiler's scanner calls and an extension implements for
/// [`ExtensionBuilder::encodings`].
pub use php_compile::multibyte::{Encoding, Encodings, Unicode};

/// What an extension hands the engine with [`ExtensionBuilder::encodings`]:
/// php's `zend_multibyte_functions` ([`Encodings`]), which the scanner
/// calls, with the two answers rfc1867 asks through
/// `php_rfc1867_set_multibyte_callbacks` besides them (`Z/main/rfc1867.c`).
pub trait Multibyte: Encodings {
    /// `php_rfc1867_encoding_translation` with `php_rfc1867_get_detect_order`:
    /// the encodings a multipart body's header lines are detected among
    /// where the body is translated, empty for the detect order, and
    /// `None` where it is not translated.
    fn http_input(&self) -> Option<Vec<Encoding>>;
    /// `php_mb_mbchar_bytes`: how many bytes the character that opens
    /// with `lead` takes in `encoding`, which the form's words step by.
    fn char_len(&self, encoding: Encoding, lead: u8) -> usize;
}

/// The startup ini view an extension's engine state is built from: what
/// the host set before the first request, immutable afterwards, each
/// entry's value `None` where it has none (php's `NULL`).
#[derive(Debug, Clone, Default)]
pub struct StartupIni {
    pub entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

/// What an extension's engine start is handed, php's `MINIT` as it runs
/// inside `php_module_startup` (ADR-0010): the startup ini, the host's
/// name, `sapi_module.name`, and the process states. Read through its
/// accessors, so a later field is not a breaking change.
#[derive(Debug)]
pub struct Start {
    pub(crate) ini: StartupIni,
    pub(crate) host: &'static str,
    /// The process states of this extension and of every one before it
    /// in bundle order, as each was started for the process.
    pub(crate) process: Vec<crate::process_scope::ProcessState>,
}

impl Start {
    /// The startup ini, as the engine's entries stand once every
    /// handler has taken what the host set at the startup stage: a
    /// host's own entries are laid over the started engine at the
    /// activate stage (`Engine::alter_master_ini`), after every engine
    /// start, as php-fpm's pool values are not in the ini its master's
    /// `MINIT` read.
    #[must_use]
    pub fn ini(&self) -> &StartupIni {
        &self.ini
    }

    /// The host's name, what `php_sapi_name()` answers: `cli`,
    /// `cgi-fcgi`, `fpm-fcgi`, `phpdbg`, empty where no host named
    /// itself.
    #[must_use]
    pub fn host(&self) -> &str {
        self.host
    }

    /// The process state of type `P` (ADR-0010), what an
    /// [`ExtensionBuilder::process_state`] of this extension or of one
    /// before it in bundle order built, once for the process; an engine
    /// state that needs it past its start holds the reference. `None`
    /// for a type no such extension built.
    #[must_use]
    pub fn process<P: Send + Sync + 'static>(&self) -> Option<&'static P> {
        self.process
            .iter()
            .find_map(|&state| state.downcast_ref::<P>())
    }
}

/// An engine start that failed, php's `MINIT` returning `FAILURE`
/// (ADR-0010): what it raised before it failed, each diagnostic at its
/// level. The engine starts no extension after it and serves nothing;
/// it displays these as php displays a startup error, then
/// `zend_startup_module_ex`'s `Unable to start %s module`
/// (`Z/Zend/zend_API.c:2465`), and the host ends the process as
/// `php_error_cb`'s `exit(-2)` does (`Z/main/main.c:1505-1507`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refusal {
    pub(crate) raised: Vec<(Level, Vec<u8>)>,
}

impl Refusal {
    /// A refusal that raised nothing before it.
    #[must_use]
    pub fn new() -> Refusal {
        Refusal::default()
    }

    /// `message` raised at `level` before the failure, after what was
    /// raised already, at `Unknown` on line 0 as php's `zend_error`
    /// places a diagnostic raised while it starts.
    #[must_use]
    pub fn raise(mut self, level: Level, message: impl Into<Vec<u8>>) -> Refusal {
        self.raised.push((level, message.into()));
        self
    }
}

/// What Core's own check of an ini value says of it, the `OnUpdate*`
/// refusals of `Z/main/main.c` that [`Request::ini_verdict`] reads.
///
/// [`Request::ini_verdict`]: crate::api::request::Request::ini_verdict
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IniVerdict {
    Accepted,
    Rejected,
}

/// One row of `zend_module_entry.deps`, a `zend_module_dep`
/// (`Z/Zend/zend_modules.h`): the other extension's name, how this one
/// stands to it, and the `rel` and `version` a `ZEND_MOD_REQUIRED_EX`
/// row carries, which php prints after the kind and
/// `ReflectionExtension::getDependencies` reads. The engine starts its
/// extensions in the order `zend_sort_modules` leaves by the required
/// and optional rows, and starts none whose required one has not
/// started before it, as `zend_startup_module_ex` refuses it. That a
/// build carries a required one at all is `api-check`'s reading of the
/// `Required` rows each extension hands
/// [`ExtensionBuilder::dependencies`], read from its sources and held
/// against `php_bundle`'s features: each feature switches on the
/// extensions its own declares required, and a crate's Cargo link with
/// no such row requires nothing (#1915).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub kind: DependencyKind,
    pub rel: Option<String>,
    pub version: Option<String>,
}

/// `MODULE_DEP_REQUIRED`, `MODULE_DEP_CONFLICTS` and
/// `MODULE_DEP_OPTIONAL`, the three kinds a `zend_module_dep` row has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Required,
    Conflicts,
    Optional,
}

/// The engine-owned, bounded, get-or-build cache of immutable values an
/// extension holds as a field of its engine state (ADR-0005): the pcre
/// pattern cache and the unit cache are its two users. The `Mutex`, the
/// bound and the eviction are here and nowhere an extension can see.
pub struct Cache<K, V> {
    capacity: usize,
    entries: Mutex<HashMap<K, Arc<V>>>,
}

impl<K: Hash + Eq + Clone, V> Cache<K, V> {
    /// A cache holding at most `capacity` values; at the bound half are
    /// dropped, php's partial clean.
    #[must_use]
    pub fn new(capacity: usize) -> Cache<K, V> {
        Cache {
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The value for `key`, built by `build` once and shared afterwards.
    pub fn get_or_build(&self, key: K, build: impl FnOnce(&K) -> V) -> Arc<V> {
        let mut entries = self.entries();
        if let Some(found) = entries.get(&key) {
            return Arc::clone(found);
        }
        self.make_room(&mut entries);
        let value = Arc::new(build(&key));
        entries.insert(key, Arc::clone(&value));
        value
    }

    /// php's partial clean at the bound: half the values dropped.
    fn make_room(&self, entries: &mut HashMap<K, Arc<V>>) {
        if entries.len() >= self.capacity {
            let keep = self.capacity / 2;
            let keys: Vec<K> = entries.keys().skip(keep).cloned().collect();
            for k in keys {
                entries.remove(&k);
            }
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<K, Arc<V>>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many keys are held.
    pub(crate) fn len(&self) -> usize {
        self.entries().len()
    }

    /// The value held for `key`, built by nothing: what the unit cache
    /// asks before it compiles, so a hit and a miss are told apart.
    pub(crate) fn get(&self, key: &K) -> Option<Arc<V>> {
        self.entries().get(key).map(Arc::clone)
    }

    /// `value` held for `key`, or what another thread put there while
    /// this one built it, which is kept: the one both are answered. A
    /// value held there that `stale` answers true for is replaced, under
    /// every key that held it, as one bucket of php's hash that several
    /// keys point to is updated in place.
    pub(crate) fn insert_over(&self, key: K, stale: impl Fn(&V) -> bool, value: V) -> Arc<V> {
        let mut entries = self.entries();
        let replaced = match entries.get(&key) {
            Some(found) if !stale(found) => return Arc::clone(found),
            found => found.map(Arc::clone),
        };
        let value = Arc::new(value);
        let Some(replaced) = replaced else {
            self.make_room(&mut entries);
            entries.insert(key, Arc::clone(&value));
            return value;
        };
        for held in entries.values_mut() {
            if Arc::ptr_eq(held, &replaced) {
                *held = Arc::clone(&value);
            }
        }
        value
    }

    /// `value`, held already, held under `key` too, in place of what
    /// the key held.
    pub(crate) fn alias(&self, key: K, value: &Arc<V>) {
        let mut entries = self.entries();
        if entries
            .get(&key)
            .is_some_and(|found| Arc::ptr_eq(found, value))
        {
            return;
        }
        self.make_room(&mut entries);
        entries.insert(key, Arc::clone(value));
    }

    /// Every value held, with its key, in no order.
    pub(crate) fn held(&self) -> Vec<(K, Arc<V>)> {
        self.entries()
            .iter()
            .map(|(key, value)| (key.clone(), Arc::clone(value)))
            .collect()
    }
}

/// The engine-owned pool of persistent resources (ADR-0005's amendment of
/// design ledger 51, #1172; design §4.10): php's `EG(persistent_list)`,
/// one list per php-fpm process or per ZTS thread, as entries lent to one
/// request at a time. A request takes an idle entry out by php's key,
/// holds it alone for the rest of the request, and gives it back at its
/// end unless the program closed it; two requests asking one key at once
/// hold two entries, as two php-fpm workers hold two connections. `V`
/// holds no request memory, only what the connection is: the socket, the
/// `transport`'s and the crypto layer's state, a database connection.
/// The `Mutex` and the bound are here and nowhere an extension can see.
pub struct Pool<K, V> {
    capacity: usize,
    idle: Mutex<Idle<K, V>>,
}

/// What a [`Pool`]'s `Mutex` guards: the idle entries by key, each with
/// the number of its give-back, and whether the pool was drained.
struct Idle<K, V> {
    entries: HashMap<K, Vec<(u64, V)>>,
    given: u64,
    drained: bool,
}

impl<K: Hash + Eq, V> Pool<K, V> {
    /// A pool keeping at most `capacity` idle entries; one given back past
    /// the bound is answered to the caller to close ([`give_back`]). The
    /// stream layer's in its engine state, and an extension's as a field of
    /// its engine state, as persistent PDO's is; what a request took out it
    /// holds in a [`Lent`] of its request state.
    ///
    /// [`give_back`]: Pool::give_back
    #[must_use]
    pub fn new(capacity: usize) -> Pool<K, V> {
        Pool {
            capacity: capacity.max(1),
            idle: Mutex::new(Idle {
                entries: HashMap::new(),
                given: 0,
                drained: false,
            }),
        }
    }

    fn idle(&self) -> std::sync::MutexGuard<'_, Idle<K, V>> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An idle entry under `key`, lent to the caller alone: the one given
    /// back last, as php's one entry per key is the one a worker last used.
    pub fn take(&self, key: &K) -> Option<V> {
        let mut idle = self.idle();
        let entries = idle.entries.get_mut(key)?;
        let entry = entries.pop();
        if entries.is_empty() {
            idle.entries.remove(key);
        }
        entry.map(|(_, entry)| entry)
    }

    /// The entries idle under every key, which php counts as a process's
    /// inactive persistent links, `MyG(num_inactive_persistent)`.
    #[must_use]
    pub fn idle_len(&self) -> usize {
        self.idle().entries.values().map(Vec::len).sum()
    }

    /// `entry` given back under `key` at the end of the request that
    /// held it; past the bound, or once the pool is drained, it is
    /// answered to the caller, the extension that owns it, which closes it
    /// with the ending request, outside the `Mutex` (ADR-0010's amendment
    /// of ADR-0005): an entry leaves the pool only through its extension,
    /// never by a drop no request sees, and one a request on another
    /// thread held as the engine ended is closed with that request.
    #[must_use = "an entry past the bound is the caller's to close with the ending request"]
    pub fn give_back(&self, key: K, entry: V) -> Option<V> {
        let mut idle = self.idle();
        if idle.drained || idle.entries.values().map(Vec::len).sum::<usize>() >= self.capacity {
            return Some(entry);
        }
        idle.given += 1;
        let given = idle.given;
        idle.entries.entry(key).or_default().push((given, entry));
        None
    }

    /// Every idle entry, with its key, taken out for the caller to close
    /// outside the `Mutex`, the one given back last first, as
    /// `zend_hash_graceful_reverse_destroy` destroys `EG(persistent_list)`
    /// last entry first; from here every entry given back is answered to
    /// its caller ([`give_back`](Pool::give_back)). What an extension's
    /// engine-end function does with its pools (ADR-0010), and the stream
    /// layer with its own before any engine-end function
    /// ([`StreamHooks::engine_end`](crate::layer::StreamHooks::engine_end)), as `zend_shutdown`
    /// destroys `EG(persistent_list)` before `zend_destroy_modules`
    /// (`Z/Zend/zend.c:1166-1170`).
    #[must_use]
    pub fn drain(&self) -> Vec<(K, V)>
    where
        K: Clone,
    {
        let mut idle = self.idle();
        idle.drained = true;
        let mut drained: Vec<(u64, K, V)> = idle
            .entries
            .drain()
            .flat_map(|(key, entries)| {
                entries
                    .into_iter()
                    .map(move |(given, entry)| (given, key.clone(), entry))
            })
            .collect();
        drained.sort_unstable_by_key(|&(given, _, _)| std::cmp::Reverse(given));
        drained
            .into_iter()
            .map(|(_, key, entry)| (key, entry))
            .collect()
    }
}

/// The entries of a [`Pool`] one request holds, each under php's key from
/// the ask that took it out or opened it to the request's end: a second
/// ask by the key [`find`](Lent::find)s the one held, and the request's
/// `rshutdown` [`drain`](Lent::drain)s them to give each back. An entry
/// is reached by its place, which no later hold reuses within the
/// request, so an object or a resource can name the entry it shares.
pub struct Lent<K, V> {
    held: Vec<Option<(K, V)>>,
}

impl<K, V> Default for Lent<K, V> {
    fn default() -> Lent<K, V> {
        Lent { held: Vec::new() }
    }
}

impl<K: PartialEq, V> Lent<K, V> {
    /// The place of the entry held under `key`.
    pub fn find(&self, key: &K) -> Option<usize> {
        self.held
            .iter()
            .position(|held| held.as_ref().is_some_and(|(k, _)| k == key))
    }

    /// The entry held at `at`, `None` once drained.
    pub fn get_mut(&mut self, at: usize) -> Option<&mut V> {
        self.held.get_mut(at)?.as_mut().map(|(_, entry)| entry)
    }

    /// `entry` held under `key` for the rest of the request; its place.
    pub fn hold(&mut self, key: K, entry: V) -> usize {
        self.held.push(Some((key, entry)));
        self.held.len() - 1
    }

    /// Every entry held, with its key, taken out for the caller to give
    /// back to its pool: what the request's `rshutdown` does.
    pub fn drain(&mut self) -> Vec<(K, V)> {
        self.held.iter_mut().filter_map(Option::take).collect()
    }
}

/// A Zend extension's `zend_extension` row (`Z/Zend/zend_extensions.h`),
/// for an extension php loads both as a `zend_module_entry` and as a Zend extension, as
/// opcache's `zend_extension_entry` stands beside its
/// `opcache_module_entry`: what `php -m` lists under `[Zend Modules]`,
/// `get_loaded_extensions(true)` answers and `ReflectionZendExtension`
/// reads, and the handlers php's struct carries beside them.
#[derive(Debug, Clone)]
pub struct ZendExtension {
    pub name: String,
    pub version: String,
    pub author: String,
    pub url: String,
    pub copyright: String,
    /// `statement_handler`: told at each statement of a unit compiled
    /// with extended info, before it runs ([`Statement`]); every Zend
    /// extension's in the order they start.
    pub statement_handler: Option<StatementHandler>,
}

pub(crate) use crate::api::__private::Entry;
/// Builds one extension's request state at request start.
pub(crate) type StateBuilder = (TypeId, fn() -> Box<dyn Any + Send>);

/// What [`ExtensionBuilder::constants_with`] was handed: the declared
/// constants rewritten with every value the bundle registered.
pub(crate) type ConstantsLink =
    Arc<dyn Fn(Vec<ConstDecl>, &Keyed<TypeId, Registered>) -> Vec<ConstDecl> + Send + Sync>;
/// Builds one extension's process state, of the type named beside it,
/// or refuses to: called at most once per process
/// ([`crate::process_scope`]).
pub(crate) type ProcessStateBuilder = (
    TypeId,
    Arc<dyn Fn() -> Result<Box<dyn Any + Send + Sync>, Refusal> + Send + Sync>,
);
/// Builds one extension's engine state as the engine starts, once, or
/// refuses to.
pub(crate) type EngineStateBuilder =
    Arc<dyn Fn(&Start) -> Result<Box<dyn Any + Send + Sync>, Refusal> + Send + Sync>;
/// The `RINIT` function.
pub(crate) type Rinit = fn(&mut Request) -> Result<(), Thrown>;
/// The function of one end phase, `RSHUTDOWN`, `deactivate`,
/// `post_deactivate` or the engine end: served in reverse bundle order,
/// each in a `zend_try` of its own (ADR-0010).
pub(crate) type EndFunction = fn(&mut Request);
/// The interrupt hook, php's `zend_interrupt_function`:
/// [`ExtensionBuilder::interrupt`].
pub(crate) type InterruptHook =
    fn(&mut Request, Beneath<'_, chain::Interrupt>) -> Result<(), Thrown>;
/// The hook `ZEND_TICKS` serves, a function php's `php_add_tick_function`
/// adds.
pub(crate) type TicksHook = fn(&mut Request) -> Result<(), Thrown>;
/// The compile hook, php's `zend_compile_file` as an extension overrides
/// it: [`ExtensionBuilder::compile_file`].
pub(crate) type CompileFileHook =
    fn(&mut Request, Beneath<'_, chain::CompileFile>) -> Result<Option<Compiled>, Thrown>;
/// The string compile hook, php's `zend_compile_string` as an extension
/// overrides it: [`ExtensionBuilder::compile_string`].
pub(crate) type CompileStringHook =
    fn(&mut Request, Beneath<'_, chain::CompileString>) -> Result<Compiled, Thrown>;
/// The error hook, php's `zend_error_cb` as an extension overrides it:
/// [`ExtensionBuilder::error`].
pub(crate) type ErrorHook = fn(&mut Request, Beneath<'_, chain::Error>);
/// The throw hook, php's `zend_throw_exception_hook`:
/// [`ExtensionBuilder::thrown`].
pub(crate) type ThrownHook = fn(&mut Request, Beneath<'_, chain::Throw>);
/// The cycle collection hook, php's `gc_collect_cycles` pointer as an
/// extension overrides it: [`ExtensionBuilder::gc_collect`].
pub(crate) type GcCollectHook = fn(&mut Request, Beneath<'_, chain::GcCollect>) -> i64;
/// The one question the unit cache asks an extension about a file
/// (design §6.3): [`ExtensionBuilder::admission`].
pub(crate) type AdmissionHook = fn(&mut Request, &[u8]) -> Admission;
/// What the engine asks each extension as it finishes starting up:
/// [`ExtensionBuilder::post_startup`].
pub(crate) type PostStartupHook = fn(&mut Request, Beneath<'_, chain::PostStartup>) -> PostStartup;
/// What the unit cache tells each extension of a file it holds:
/// [`ExtensionBuilder::cached`].
pub(crate) type CachedHook = fn(&mut Request, &[u8]);
/// What the unit cache hands each file where php's optimizer would
/// optimize it: [`ExtensionBuilder::optimizer_pass`].
pub(crate) type OptimizerPass = fn(&mut Request, &[u8]);
/// The host's log for a fatal: [`ExtensionBuilder::fatal_logged`].
pub(crate) type FatalLoggedHook = fn(&mut Request) -> AfterLog;

/// How a host left the fatal its log took (`sapi_module.log_message`),
/// which php's `php_error_cb` calls before it displays the fatal, writes
/// the status 255 and bails out (`Z/main/main.c:1430-1520`): the log
/// returned, or bailed out from inside itself, as phpdbg's `quit` at the
/// prompt it stands at there leaves by `zend_bailout()`
/// (`Z/sapi/phpdbg/phpdbg_prompt.c:1513`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterLog {
    /// `php_error_cb` goes on: the fatal displayed, the status 255, and
    /// the request leaves for the `zend_try` that catches the fatal.
    Returned,
    /// The rest of `php_error_cb` is not served: nothing displayed, the
    /// status as it was, and the request leaves for that `zend_try` all
    /// the same.
    BailedOut,
}

/// The CLI's interactive shell, php's `cli_shell_callbacks_t`
/// (`Z/sapi/cli/cli.h:31`), as `readline_cli.c`'s `MINIT` fills the
/// table the CLI's `php_cli_get_shell_callbacks` hands it: what an
/// extension provides with [`ExtensionBuilder::shell`] and a host that
/// carries the table serves, php's `-a`.
#[derive(Debug, Clone, Copy)]
pub struct Shell {
    /// `cli_shell_run`: the shell served in the request the host began,
    /// after `auto_prepend_file`; what it answers is the request's exit
    /// status, as `do_cli` sets `EG(exit_status)` to it.
    pub serve: fn(&mut Request) -> i32,
    /// `cli_shell_ub_write` and `cli_shell_write`, which
    /// `sapi_cli_ub_write` calls, in that order, before each body write
    /// reaches the host and whatever the host serves: the bytes seen,
    /// never kept from it, as readline's two answer `-1` and nothing.
    pub write: fn(&mut Request, &[u8]),
}

/// An extension's answer as the engine finishes starting up, before it
/// serves its first request (design §6.3): php's `zend_post_startup_cb`,
/// which opcache's `accel_post_startup` is. The engine is built before
/// any host names itself, so it asks once, on the first request's
/// host, from the top of the chain ([`ExtensionBuilder::post_startup`]):
/// what the top link answers decides, and beneath every link the chain
/// answers [`PostStartup::Serve`].
#[derive(Debug)]
pub enum PostStartup {
    /// Serve as built.
    Serve,
    /// `accel_preload`: the file at `script` compiled and served once, in
    /// a request of its own, before the first request, and every class
    /// and function it declared that links declared in every request
    /// after, as they stand in the unit cache. A file that ends in a
    /// fatal or an `exit` fails php's startup, and the process ends with
    /// status 1.
    Preload {
        /// The file, as `opcache.preload` names it.
        script: Vec<u8>,
        /// The thread of its own the request is served on, or `None` to
        /// serve it on the thread that finishes the startup.
        thread: Option<PreloadThread>,
    },
    /// Serve nothing: the process ends with `status`, as
    /// `zend_accel_error` at `ACCEL_LOG_FATAL` ends it with `exit(-2)`.
    Refuse { status: i32 },
}
/// The thread a preload is served on where the process's own may not
/// serve it, php's preload child as `accel_finish_startup_preload_subprocess`
/// forks it (`Z/ext/opcache/ZendAccelerator.c:5020-5040`), and what that
/// thread takes before it serves any PHP: `take`, handed the preload's
/// request on the thread, answers whether it took it, and `false` fails
/// the startup with status 1, as the child's `exit(1)` does, the file
/// never served. The thread that finishes the startup waits for it, as
/// php's parent `waitpid`s; the thread starts no thread of its own,
/// since one would inherit what it took, and it ends with the preload,
/// so what `take` took ends with it and the process's other threads
/// never have it. Where the system refuses the thread, `unstarted` says
/// so on the finishing thread's request, as php's refused `fork()` is
/// its own fatal, and answers the status the process ends with. What
/// [`PreloadThread::joined`] hands is done on the finishing thread once
/// the thread has ended, whatever the preload answered.
pub struct PreloadThread {
    take: Box<dyn FnOnce(&mut Request) -> bool + Send>,
    unstarted: fn(&mut Request) -> i32,
    joined: Option<Box<dyn FnOnce() + Send>>,
}

impl PreloadThread {
    /// The thread that takes what `take` takes before the preload, and
    /// `unstarted` where the system refuses it.
    pub fn new(
        take: impl FnOnce(&mut Request) -> bool + Send + 'static,
        unstarted: fn(&mut Request) -> i32,
    ) -> PreloadThread {
        PreloadThread {
            take: Box::new(take),
            unstarted,
            joined: None,
        }
    }

    /// `after` done on the thread that finishes the startup once the
    /// preload's thread has ended: what only the process may put back
    /// of what the take changed for it, such as Linux's dumpable setting,
    /// which a thread's change of ids resets for the whole process.
    #[must_use]
    pub fn joined(mut self, after: impl FnOnce() + Send + 'static) -> PreloadThread {
        self.joined = Some(Box::new(after));
        self
    }

    /// What is done once the thread has ended, taken out.
    pub(crate) fn after_join(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        self.joined.take()
    }

    /// What the thread takes, taken on the thread, on `req`.
    pub(crate) fn take(self, req: &mut Request) -> bool {
        (self.take)(req)
    }

    /// What answers a thread the system refused.
    pub(crate) fn unstarted(&self) -> fn(&mut Request) -> i32 {
        self.unstarted
    }
}

impl std::fmt::Debug for PreloadThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreloadThread")
    }
}

/// The path resolver, php's `zend_resolve_path` as an extension overrides
/// it: [`ExtensionBuilder::resolve_path`].
pub(crate) type ResolvePathHook = fn(
    &mut Request,
    Beneath<'_, chain::ResolvePath>,
) -> Result<Vec<u8>, crate::api::request::Unresolved>;

/// What [`ExtensionBuilder::encodings`] takes: the encoding functions
/// bound to one request.
pub(crate) type EncodingsHook = fn(&mut Request) -> Box<dyn Multibyte>;
/// The upload hook, php's `php_rfc1867_callback`:
/// [`ExtensionBuilder::upload`].
pub(crate) type UploadHook = fn(&mut Request, Beneath<'_, chain::Upload>) -> Result<bool, Thrown>;
/// The data treater, php's `sapi_module.treat_data`:
/// [`ExtensionBuilder::treat_data`].
pub(crate) type TreatDataHook = fn(
    &mut Request,
    crate::api::request::Input,
    &[u8],
) -> Result<Option<Vec<(Vec<u8>, Vec<u8>)>>, Thrown>;
/// The input filter, php's `sapi_module.input_filter`:
/// [`ExtensionBuilder::input_filter`].
pub(crate) type InputFilterHook =
    fn(
        &mut Request,
        crate::api::request::Input,
        &[u8],
        &[u8],
    ) -> Result<Option<crate::api::values::Owner<crate::api::values::Value>>, Thrown>;

/// What the unit cache does with a file for the request asking, the
/// answer to [`ExtensionBuilder::admission`] (design §6.3, revision 45):
/// what `persistent_compile_file` decides from `accelerator_enabled`,
/// the blacklist and `opcache.validate_timestamps`,
/// `opcache.revalidate_freq`, `opcache.revalidate_path` and
/// `opcache.file_cache`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Kept out: compiled as the request's own unit, published nowhere
    /// and counted among the cache's blacklist misses, as
    /// `accel_blacklist` and `opcache.max_file_size` keep a file out.
    KeptOut,
    /// Held, a hit validated as `validation` says and looked for as
    /// `lookup` says; a file compiled at a request that starts within
    /// `protection` seconds of the file's last write is compiled and not
    /// held, `opcache.file_update_protection`, since the second a file
    /// was written in cannot tell a later write in the same second. With
    /// a `file_cache`, a miss is looked for there before it is compiled,
    /// and what is compiled is kept there too.
    Held {
        validation: Validation,
        lookup: Lookup,
        protection: u32,
        file_cache: Option<FileCache>,
    },
    /// Kept in the `file_cache` alone and held in no memory, which
    /// `file_cache_compile_file` is: `opcache.file_cache_only`, or a
    /// request whose accelerator is off beside a file cache. A file kept
    /// there is read back as `validation` validates it, never looking
    /// at the file except to compare its `mtime` in whole seconds, and
    /// one compiled within `protection` seconds of its last write is not
    /// kept.
    OnDisk {
        validation: Validation,
        protection: u32,
        file_cache: FileCache,
    },
}

/// `opcache.file_cache`, the unit cache's second level (design §6.3):
/// the directory the compiled files are kept under, each at the path
/// `zend_file_cache_get_bin_file_path` makes of it, and whether the
/// cache may only read there, `opcache.file_cache_read_only`. What is
/// kept is this build's alone; another build's files are never read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCache {
    pub directory: std::path::PathBuf,
    pub read_only: bool,
}

/// When a hit on a held file is held to the file as it stands.
/// A held unit is never changed: one that fails is compiled again and
/// replaced, and whoever holds the old one keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// Never: `opcache.validate_timestamps=0`, which serves a file as it
    /// was when it was cached, changed or deleted, until it is
    /// invalidated.
    Never,
    /// Once per `seconds`, `validate_timestamp_and_record` under
    /// `opcache.revalidate_freq`: the file's `mtime` in whole seconds
    /// held to the one it was cached with, and not again until the
    /// window after the request that validated it has passed; zero
    /// validates every hit.
    Window { seconds: u32 },
    /// Every hit, against the file's `mtime` to the nanosecond and its
    /// size: php without the cache, which compiles a file afresh each
    /// time, and what a request whose accelerator is off, or a bundle
    /// with no extension that answers, gets.
    EveryHit,
}

/// How a hit is looked for, `opcache.revalidate_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// By the path as written first, the working directory, the
    /// `include_path` and the including file's directory beside a
    /// relative one, and trusted without resolving the path where the
    /// hit is not due to be validated (`accel_make_persistent_key`):
    /// `revalidate_path=0`.
    AsWritten,
    /// By the resolved path alone: `revalidate_path=1`.
    Resolved,
}

/// An ini entry's `on_modify`, php's `PHP_INI_MH`: served by `ini_set()`
/// with the new value before the entry takes it
/// (`ZEND_INI_STAGE_RUNTIME`), by the engine as it is built with the
/// value the host set (`ZEND_INI_STAGE_STARTUP`), by a host laying its
/// own entry over php once it has started (`ZEND_INI_STAGE_ACTIVATE`),
/// and as the request ends with the value an entry the request changed
/// started with (`ZEND_INI_STAGE_DEACTIVATE`); [`Ini::stage`]
/// says which. The value is php's `new_value`, `None` where it has none
/// (`NULL`), which an entry registered with no value is handed back as
/// the request ends or `ini_restore()` restores it, and which a rule may
/// read apart from the empty string. It raises what the entry's handler
/// raises, and answers whether the value is taken, `false` being php's
/// `FAILURE`, which leaves the entry as it was, its default at startup,
/// and makes `ini_set()` answer `false`.
pub type OnModify = fn(&mut Request, Option<&[u8]>) -> Result<bool, Thrown>;

/// php's standard handler of a whole-number entry, `OnUpdateLong` or
/// `OnUpdateLongGEZero` (`Z/Zend/zend_ini.c:972-993`), which the engine
/// serves for an entry declared with it
/// ([`ExtensionBuilder::on_update_long`]) as it serves core's: the value
/// read by `zend_ini_parse_quantity_warn`
/// ([`Ini::parse_quantity_warn`]), warning where the rule
/// interprets it, at startup, at `ini_set()` and as the request ends,
/// and read back with [`Ini::long`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongHandler {
    /// `OnUpdateLong`: any value is taken.
    Any,
    /// `OnUpdateLongGEZero`: a negative value is refused, and the entry
    /// keeps what it held.
    AtLeastZero,
}

impl LongHandler {
    /// Whether the handler takes the quantity `value` read as.
    pub(crate) fn takes(self, value: i64) -> bool {
        self == LongHandler::Any || value >= 0
    }
}

/// An entry's handler as the engine holds it, php's `on_modify` slot:
/// one the extension wrote, or php's standard whole-number one, which
/// the engine serves. An entry has at most one.
#[derive(Clone, Copy)]
pub(crate) enum IniHandler {
    Written(OnModify),
    Long(LongHandler),
}

impl IniHandler {
    /// The handler served for the entry `name` taking `value`: whether
    /// it takes it.
    pub(crate) fn serve(
        self,
        req: &mut Request,
        name: &[u8],
        value: Option<&[u8]>,
    ) -> Result<bool, Thrown> {
        match self {
            IniHandler::Written(handler) => handler(req, value),
            IniHandler::Long(handler) => {
                let quantity = req
                    .ini_mut()
                    .parse_quantity_warn(name, value.unwrap_or_default());
                Ok(handler.takes(quantity))
            }
        }
    }
}

/// The stage an [`OnModify`] is served at, php's `ZEND_INI_STAGE_*`
/// (`Z/Zend/zend_ini.h`) as this engine reaches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniStage {
    /// `ZEND_INI_STAGE_STARTUP`: as the engine is built, over the
    /// host's values, before any request; `php_during_module_startup()`.
    Startup,
    /// `ZEND_INI_STAGE_ACTIVATE`: a host's own entry laid over php once
    /// it has started, as php-fpm's child applies its pool's
    /// `php_value`s.
    Activate,
    /// `ZEND_INI_STAGE_RUNTIME`: while a request runs, through
    /// `ini_set()`, `ini_restore()` and the natives.
    Runtime,
    /// `ZEND_INI_STAGE_DEACTIVATE`: as the request ends, each entry it
    /// changed handed back the value it started with
    /// (`zend_ini_deactivate`).
    Deactivate,
}
/// The `MINFO` function: writes the extension's `phpinfo()` table
/// through [`Extensions::info_table`].
pub(crate) type Info = fn(&mut Request);
/// A native class as an extension declares it (design §7.1): the
/// declaration, beside it each method's declaration with its entry,
/// the shape `functions` has, and the class's handler table (design
/// §5.4), none where the class takes its parent's, as php's
/// `create_object` is inherited, and [`Std`]'s at a root. The methods
/// given here are the class's; the engine places them on the
/// declaration as it links it, so the declaration's own `methods` is
/// left empty by an extension. A method's entry is served as a
/// function's is, with the receiver on the frame as `Frames::this`
/// (ADR-0002). The fourth is the class's `create_object` where its
/// objects carry a native payload, [`NativeNew`].
pub(crate) type NativeClassDecl = (
    ClassDecl,
    Vec<(MethodDecl, Entry)>,
    Option<&'static dyn ObjectHandlers>,
    Option<NativeNew>,
);

/// `ce->create_object` for a class whose objects carry a native payload
/// (design §5.4), php's `zend_object_alloc(sizeof(custom), ce)`: the
/// allocation sized for the payload with the payload's `Default`
/// already written, so the slot stands before the constructor runs and
/// a method reaches it through [`PhpObject::native_mut`]. `None` is
/// php's null slot, a class whose objects are `zend_objects_new` and
/// carry nothing; the engine takes the nearest parent's, as php
/// inherits `create_object`.
pub(crate) type NativeNew = fn(&mut Heap, u32, u32, usize) -> Result<Obj, Fatal>;

/// One [`NativeNew`], the payload's `Default` into a fresh object of
/// `class` with `slots` declared property slots.
/// The `create_object` of a native class of the engine's own whose
/// objects carry a `T`, as [`Class::new_payload`] gives an
/// extension's.
pub(crate) fn new_payload<T: Any + Send + Default>() -> NativeNew {
    new_with_payload::<T> as NativeNew
}

fn new_with_payload<T: Any + Send + Default>(
    heap: &mut Heap,
    class: u32,
    slots: u32,
    custom: usize,
) -> Result<Obj, Fatal> {
    heap.object_with(class, slots, custom, T::default())
}

/// One extension (design §7.1): what it declares into the engine's
/// tables ([`Declarations`]) and the rest of its entry, the ini
/// entries, lifecycle functions and state declarations
/// ([`ExtensionEntry`]). Built by [`ExtensionBuilder`], linked by the
/// engine, which moves the declarations into its own tables and keeps
/// the entry.
#[derive(Clone)]
pub struct Extension {
    pub(crate) entry: ExtensionEntry,
    pub(crate) declarations: Declarations,
}

impl Extension {
    /// The extension as the tier's: its ini entries, states and
    /// lifecycle served as any extension's, and unlisted in
    /// `module_registry` ([`ExtensionEntry::listed`]).
    pub(crate) fn unlisted(mut self) -> Extension {
        self.entry.listed = false;
        self
    }
}

/// One extension as the engine holds it once built, php's
/// `zend_module_entry` with its function table registered and gone: the
/// name, version, ini entries, lifecycle functions and the two state
/// declarations. Its functions, classes and constants are rows of the
/// engine's tables, each naming the extension that declared it.
#[derive(Clone)]
pub(crate) struct ExtensionEntry {
    pub(crate) name: String,
    /// Whether the entry stands in `module_registry` as a program reads
    /// it, listed and found by name: `get_loaded_extensions()`,
    /// `extension_loaded()`, `ReflectionExtension`, `--re`, `--ri` and
    /// `phpinfo()`. Every extension's does, and the tier's does not
    /// ([`Extension::unlisted`]): the tier declares its state as an
    /// extension declares it and is no extension (`jit-design.md` §4),
    /// as php's tracing tier is part of `Zend OPcache` and has no
    /// `zend_module_entry` of its own.
    pub(crate) listed: bool,
    /// `zend_module_entry.version`, what [`ExtensionBuilder::version`]
    /// declared, `PHP_VERSION` where it declared none.
    pub(crate) version: &'static str,
    pub(crate) ini: Vec<IniDecl>,
    pub(crate) on_modify: Vec<(Vec<u8>, IniHandler)>,
    pub(crate) dependencies: Vec<Dependency>,
    pub(crate) request_state: Option<StateBuilder>,
    pub(crate) process_state: Option<ProcessStateBuilder>,
    pub(crate) engine_state: Option<EngineStateBuilder>,
    pub(crate) rinit: Option<Rinit>,
    pub(crate) rshutdown: Option<EndFunction>,
    /// What [`ExtensionBuilder::deactivate`] was handed.
    pub(crate) deactivate: Option<EndFunction>,
    /// What [`ExtensionBuilder::post_deactivate`] was handed.
    pub(crate) post_deactivate: Option<EndFunction>,
    /// What [`ExtensionBuilder::engine_end`] was handed.
    pub(crate) engine_end: Option<EndFunction>,
    /// What [`ExtensionBuilder::zend_extension`] was handed.
    pub(crate) zend: Option<ZendExtension>,
    /// What the builder methods of the supplied kinds were handed
    /// ([`crate::supplied`]), taken out as the engine is built.
    pub(crate) supplies: Supplies,
    pub(crate) info: Option<Info>,
}

/// What one extension declares into the engine's tables, which the
/// engine's build moves into them row by row, so no declaration is held
/// twice: its functions, classes and constants, and what it adds to the
/// classes of others.
#[derive(Clone)]
pub(crate) struct Declarations {
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    pub(crate) classes: Vec<NativeClassDecl>,
    pub(crate) constants: Vec<ConstDecl>,
    /// What [`ExtensionBuilder::constants_with`] was handed.
    pub(crate) constants_link: Option<ConstantsLink>,
    /// What [`ExtensionBuilder::class_constants`] was handed: each
    /// class another extension declares, with the constants this one
    /// adds to it.
    pub(crate) class_constants: Vec<(Name, Vec<ClassConstDecl>)>,
    /// What [`ExtensionBuilder::trampolines`] was handed: each class,
    /// with the methods its `get_method` answers off its table.
    pub(crate) trampolines: Vec<(Name, MethodDecl, Entry)>,
}

/// An extension handed by reference to
/// [`crate::engine::EngineBuilder::extensions`], cloned, as a host that
/// builds more than one engine from one bundle hands it.
impl From<&Extension> for Extension {
    fn from(extension: &Extension) -> Extension {
        extension.clone()
    }
}

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extension")
            .field("name", &self.entry.name)
            .field("functions", &self.declarations.functions.len())
            .field("classes", &self.declarations.classes.len())
            .finish_non_exhaustive()
    }
}

/// The states of one scope, one per extension that declared one, held
/// in bundle order and dropped in its reverse (ADR-0005, ADR-0010), as
/// php's `zend_destroy_modules` walks the modules back to front. A
/// request's states and an engine's are each one of these, so every
/// request a state is built for, the startup one, a refused one or a
/// served one, and every engine drops them in the one order written
/// here. A request state no extension declared, built at first touch
/// by [`Request::state`], is held after the rest and dropped first.
/// Nothing reorders what is held: it is read as a slice and written
/// one state at a time.
pub(crate) struct States<T>(Vec<T>);

impl<T> States<T> {
    pub(crate) const fn new() -> States<T> {
        States(Vec::new())
    }

    /// Hold `state` after every state held before it.
    pub(crate) fn push(&mut self, state: T) {
        self.0.push(state);
    }

    /// The state held `at` that place, to replace or change in place.
    pub(crate) fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        self.0.get_mut(at)
    }

    /// The state held at `at` taken out, the rest closing up behind it:
    /// what a test takes back from a request that ended.
    #[cfg(any(test, feature = "test"))]
    pub(crate) fn remove(&mut self, at: usize) -> T {
        self.0.remove(at)
    }
}

impl<T> FromIterator<T> for States<T> {
    fn from_iter<I: IntoIterator<Item = T>>(states: I) -> States<T> {
        States(states.into_iter().collect())
    }
}

impl<T> std::ops::Deref for States<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> Drop for States<T> {
    /// Reversed in place, so the vector's own drop, which goes on past a
    /// state whose drop unwinds, drops them back to front.
    fn drop(&mut self) {
        self.0.reverse();
    }
}

/// The builder of an [`Extension`], what a crate's `extension()` calls.
#[derive(Default)]
pub struct ExtensionBuilder {
    version: Option<&'static str>,
    functions: Vec<(FunctionDecl, Entry)>,
    classes: Vec<NativeClassDecl>,
    constants: Vec<ConstDecl>,
    ini: Vec<IniDecl>,
    on_modify: Vec<(Vec<u8>, IniHandler)>,
    dependencies: Vec<Dependency>,
    request_state: Option<StateBuilder>,
    process_state: Option<ProcessStateBuilder>,
    engine_state: Option<EngineStateBuilder>,
    rinit: Option<Rinit>,
    rshutdown: Option<EndFunction>,
    deactivate: Option<EndFunction>,
    post_deactivate: Option<EndFunction>,
    engine_end: Option<EndFunction>,
    zend: Option<ZendExtension>,
    supplies: Supplies,
    info: Option<Info>,
    constants_link: Option<ConstantsLink>,
    class_constants: Vec<(Name, Vec<ClassConstDecl>)>,
    trampolines: Vec<(Name, MethodDecl, Entry)>,
}

impl ExtensionBuilder {
    /// The natives: each declaration with its entry.
    #[must_use]
    pub fn functions(mut self, functions: Vec<(FunctionDecl, Entry)>) -> ExtensionBuilder {
        self.functions.extend(functions);
        self
    }

    /// The native classes: each declaration with its methods' entries
    /// beside it, `(ClassDecl, Vec<(MethodDecl, Entry)>)`, the shape
    /// [`ExtensionBuilder::functions`] has. The engine links each into
    /// the linked table beside the classes Zend declares (design §6.2,
    /// §7.1); a method's entry reads its arguments as a function's does
    /// and its receiver as `Frames::this`.
    #[must_use]
    pub fn classes(
        mut self,
        classes: Vec<(ClassDecl, Vec<(MethodDecl, Entry)>)>,
    ) -> ExtensionBuilder {
        self.classes.extend(
            classes
                .into_iter()
                .map(|(decl, methods)| (decl, methods, None, None)),
        );
        self
    }

    /// One native class with its own handler table (design §5.4): the
    /// declaration and its methods' entries as [`ExtensionBuilder::classes`]
    /// takes them, and beside them the [`ObjectHandlers`] the kernels
    /// dispatch to for an object of the class, `Std` for every slot the
    /// table leaves to it. A class declared through `classes` takes its
    /// parent's table, and `Std` at a root, as php inherits
    /// `create_object`.
    #[must_use]
    pub fn class_with(
        mut self,
        class: ClassDecl,
        methods: Vec<(MethodDecl, Entry)>,
        handlers: &'static dyn ObjectHandlers,
    ) -> ExtensionBuilder {
        self.classes.push((class, methods, Some(handlers), None));
        self
    }

    /// The class `C` declares with the native attributes (design §7.1):
    /// its header's declaration with its member impl's constants,
    /// properties and methods, registered as the header says, at the
    /// place the `extension()` value lists it. The `Members` bound is
    /// what makes a forgotten member impl a build error on this line.
    #[must_use]
    pub fn class<C: Header + Members>(self) -> ExtensionBuilder {
        self.class_reshaped::<C>(|_| {})
    }

    /// [`ExtensionBuilder::class`], the declaration then reshaped by
    /// what the attributes cannot say and php's `MINIT` writes by hand
    /// after `register_class_*`: a property whose type is an
    /// intersection or in disjunctive normal form
    /// (`zend_declare_typed_property` with a type list), the doc comment
    /// `@genstubs-expose-comment-block` keeps, a trait's kind, an
    /// instance method whose body reads no receiver. `reshape` sees the
    /// methods in `methods` and reshapes them in place: one it adds or
    /// takes away is not registered, since each keeps its entry. What it
    /// leaves is registered as `class` registers it.
    #[must_use]
    pub fn class_reshaped<C: Header + Members>(
        self,
        reshape: impl FnOnce(&mut ClassDecl),
    ) -> ExtensionBuilder {
        let mut class = C::header();
        let (constants, properties, mut methods) = C::members();
        class.constants.extend(constants);
        class.properties.extend(properties);
        let seen = methods.iter().map(|(method, _)| method.clone()).collect();
        let header = std::mem::replace(&mut class.methods, seen);
        reshape(&mut class);
        let reshaped = std::mem::replace(&mut class.methods, header);
        for ((method, _), reshaped) in methods.iter_mut().zip(reshaped) {
            *method = reshaped;
        }
        class.properties = class
            .properties
            .into_iter()
            .map(declared_property)
            .collect();
        C::register(self, class, methods)
    }

    /// One native class with its handler table and its `create_object`,
    /// what `__private::class_new` registers a payload class through.
    pub(crate) fn class_created(
        mut self,
        class: ClassDecl,
        methods: Vec<(MethodDecl, Entry)>,
        handlers: &'static dyn ObjectHandlers,
        new: NativeNew,
    ) -> ExtensionBuilder {
        self.classes
            .push((class, methods, Some(handlers), Some(new)));
        self
    }

    /// The constants.
    #[must_use]
    pub fn constants(mut self, constants: Vec<ConstDecl>) -> ExtensionBuilder {
        self.constants.extend(constants);
        self
    }

    /// The ini entries with their defaults.
    #[must_use]
    pub fn ini(mut self, entries: Vec<IniDecl>) -> ExtensionBuilder {
        self.ini.extend(entries);
        self
    }

    /// The `on_modify` of the ini entry `name`, one of the extension's
    /// own, served whenever a program changes it with `ini_set()`, and
    /// once over the value the host set as the engine is built.
    #[must_use]
    pub fn on_modify(mut self, name: &[u8], handler: OnModify) -> ExtensionBuilder {
        self.on_modify
            .push((name.to_vec(), IniHandler::Written(handler)));
        self
    }

    /// The ini entry `name`, one of the extension's own, declared with
    /// php's `OnUpdateLong` or `OnUpdateLongGEZero` ([`LongHandler`]),
    /// which the engine serves where [`ExtensionBuilder::on_modify`]'s
    /// handler would be. The extension reads the value with
    /// [`Ini::long`].
    #[must_use]
    pub fn on_update_long(mut self, name: &[u8], handler: LongHandler) -> ExtensionBuilder {
        self.on_modify
            .push((name.to_vec(), IniHandler::Long(handler)));
        self
    }

    /// The `deps` table, `ZEND_MOD_REQUIRED("libxml")` and its kin, in
    /// the order the extension lists them; an extension that declares
    /// none has php's null pointer, the empty table.
    #[must_use]
    pub fn dependencies(mut self, dependencies: Vec<Dependency>) -> ExtensionBuilder {
        self.dependencies.extend(dependencies);
        self
    }

    /// The `MINFO` function, what `phpinfo()`, `php --ri` and
    /// `ReflectionExtension::info` print for the extension, written
    /// through [`Extensions::info_table`]; an extension without one gets
    /// php's `Version` row and its ini entries.
    #[must_use]
    pub fn info(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.info = Some(hook);
        self
    }

    /// Declare request state: built at request start in bundle order,
    /// dropped after the `rshutdown` hooks in reverse (ADR-0005).
    #[must_use]
    pub fn request_state<T: Default + Send + 'static>(mut self) -> ExtensionBuilder {
        self.request_state = Some((TypeId::of::<T>(), || Box::new(T::default())));
        self
    }

    /// Declare process state, the process start (ADR-0010): built at
    /// most once per process, the first time an engine carrying the
    /// extension is built, in bundle order and before the extension's
    /// engine start, which reaches it through [`Start::process`] as a
    /// `&'static P`; held by the engine, never by the extension, and
    /// never ended, since no host ends the process scope (process exit
    /// releases it). It sees nothing an engine is configured with, so two
    /// engines built differently cannot race to define it: where C
    /// library global initialisation lives, `xmlInitParser` and its kin.
    /// A [`Refusal`] is php's `MINIT` failing on its library, which php
    /// meets once per process and answers by ending the process: it is
    /// remembered for the process, so every engine carrying the
    /// extension is refused with it, and the function is not called again.
    /// Held for the extension by its name and `P`, so each extension's is
    /// started on its own; [`Start::process`] finds the first of a type,
    /// as [`Request::engine`] does, so the type is the extension's own.
    /// Declared twice, the later stands.
    #[must_use]
    pub fn process_state<P: Send + Sync + 'static>(
        mut self,
        build: fn() -> Result<P, Refusal>,
    ) -> ExtensionBuilder {
        self.process_state = Some((
            TypeId::of::<P>(),
            Arc::new(move || build().map(|state| Box::new(state) as Box<dyn Any + Send + Sync>)),
        ));
        self
    }

    /// Declare engine state, the engine start (ADR-0010): built once as
    /// the engine is built, in bundle order, from what [`Start`] carries,
    /// immutable afterwards, shared by every request, and dropped with the
    /// engine's last clone in reverse bundle order (ADR-0005). A
    /// [`Refusal`] is php's `MINIT` failing: no later extension starts and
    /// the engine serves nothing. Declared twice, the later stands.
    #[must_use]
    pub fn engine_state<E: Send + Sync + 'static>(
        mut self,
        build: fn(&Start) -> Result<E, Refusal>,
    ) -> ExtensionBuilder {
        self.engine_state = Some(Arc::new(move |start: &Start| {
            build(start).map(|state| Box::new(state) as Box<dyn Any + Send + Sync>)
        }));
        self
    }

    /// The request-start function, `RINIT`, served in bundle order as the
    /// request starts. What it raises is php's fatal inside `RINIT`
    /// (#1775): the throwable rendered as the uncaught fatal with no frame
    /// standing, no later extension's `rinit` and no program served, no
    /// request-end phase but under the built-in server, and the host
    /// ending as php's does after `php_request_startup` fails.
    #[must_use]
    pub fn rinit(mut self, hook: fn(&mut Request) -> Result<(), Thrown>) -> ExtensionBuilder {
        self.rinit = Some(hook);
        self
    }

    /// The request-end function, `RSHUTDOWN`, served with every extension's
    /// request state still live, in reverse bundle order and each in a
    /// `zend_try` of its own: a fatal it raises ends this function only,
    /// and the next extension's is still served.
    #[must_use]
    pub fn rshutdown(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.rshutdown = Some(hook);
        self
    }

    /// Served after every extension's `rshutdown`, where php's
    /// `zend_deactivate` reaches `shutdown_executor`
    /// (`Z/Zend/zend.c:1352`), before the open resources are closed
    /// (`Z/Zend/zend_execute_API.c:277`) and the objects freed: every
    /// extension's hook in reverse bundle order, with every request
    /// state still live. What php frees with the request's objects and
    /// resources, which another extension's `RSHUTDOWN` may still use,
    /// such as a link a session save handler writes through.
    #[must_use]
    pub fn deactivate(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.deactivate = Some(hook);
        self
    }

    /// Served after the request's `shutdown_executor`, php's
    /// `post_deactivate_func` (`zend_post_deactivate_modules`, step 11 of
    /// `php_request_shutdown`, `Z/main/main.c:2026-2029`): after every
    /// `deactivate`, with the request's resources closed, its objects
    /// freed and its ini entries restored, every extension's hook in
    /// reverse bundle order and each in a `zend_try` of its own, with
    /// every request state still live. No PHP code runs here, so what it
    /// hands back to the engine, an entry lent from a pool, has outlived
    /// every object that could reach it. An end that only releases is
    /// the request state's `Drop` (ADR-0010).
    #[must_use]
    pub fn post_deactivate(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.post_deactivate = Some(hook);
        self
    }

    /// The engine-end function, php's `MSHUTDOWN` as `php_module_shutdown`
    /// serves it (`zend_destroy_modules`, `Z/Zend/zend.c:1170`): served
    /// once, from the drop of the engine's last clone, on a request the
    /// engine lays for it, the mirror of the startup request; every
    /// extension's hook in reverse bundle order and each in a `zend_try`
    /// of its own, before any engine state drops. That request reaches
    /// the engine states, builds request states as every request does,
    /// serves no program, and what it writes reaches no host. Where an
    /// extension drains its pools and closes what they hold, or
    /// finishes a file it kept open across requests (ADR-0010); an end
    /// that only releases stays the engine state's `Drop`. An engine
    /// whose start was refused is not ended.
    #[must_use]
    pub fn engine_end(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.engine_end = Some(hook);
        self
    }

    /// The interrupt hook, php's `zend_interrupt_function` as pcntl's
    /// `MINIT` sets it: served by the loop's interrupt check when the
    /// request's [`Interrupt`](crate::api::request::Interrupt) was raised
    /// by something other than the execution timer (design §5.8): signal
    /// delivery, or a periodic interrupt an extension started with
    /// [`Limits::start_periodic`], whose periods the hook reads with
    /// [`Limits::periods_elapsed`]. What
    /// it answers is left pending where the loop stands, as
    /// `zend_interrupt_helper` leaves `EG(exception)`. Two extensions'
    /// hooks chain as php's saved pointer does: the later is served
    /// first, handed the earlier ([`Beneath`]) to serve where its work
    /// is to stand, as pcntl's serves it after its own, and nothing of
    /// php's stands beneath them.
    #[must_use]
    pub fn interrupt(mut self, hook: InterruptHook) -> ExtensionBuilder {
        self.supplies.interrupt = Some(hook);
        self
    }

    /// The hook `ZEND_TICKS` serves, a function `php_add_tick_function`
    /// adds to `PG(tick_functions)` as pcntl's `MINIT` adds its signal
    /// dispatch: served each time a `declare(ticks=N)` count is reached,
    /// every extension's in bundle order and then the functions
    /// `register_tick_function()` kept, with fiber switching blocked
    /// while they are served. What it throws is left pending where the
    /// opline stands.
    #[must_use]
    pub fn ticks(mut self, hook: TicksHook) -> ExtensionBuilder {
        self.supplies.ticks = Some(hook);
        self
    }

    /// `zend_compile_file` overridden, as phar's `MINIT` overrides it
    /// with `phar_compile_file` and phpdbg with `phpdbg_compile_file`:
    /// every file compiled, by an include, `spl_autoload` or as the
    /// primary file, is served by `hook`, handed its path with the one
    /// beneath ([`Beneath`]), which it serves where its work is to stand
    /// and whose compiled file it answers or reads, or hands a source of
    /// its own to compile under the path in place of the file's
    /// (`Beneath::with_source`). Beneath every link stands the engine's
    /// cached compile, the unit cache's lookup and compile of the file,
    /// with opcache's admission deciding inside it. Two extensions' hooks
    /// chain as php's saved pointer does: the later is served first, and
    /// the earlier where the later serves the one beneath, as phar's does
    /// for a file it does not serve.
    #[must_use]
    pub fn compile_file(mut self, hook: CompileFileHook) -> ExtensionBuilder {
        self.supplies.compile_file = Some(hook);
        self
    }

    /// `zend_compile_string` overridden, as the tracers and profilers
    /// override it to time a string's compile and read what it
    /// declared: every string compiled as an `eval` compiles it, by
    /// `eval`, the CLI's `-r`, `-B`, `-R` and `-E`, the shell or the
    /// debugger, is served by `hook`, handed the one beneath
    /// ([`Beneath`]) to serve where its work is to stand, over the
    /// string's source or one of its own ([`Beneath::with_source`]),
    /// with php's own `compile_string` beneath every link. What it
    /// throws is raised where the string was to compile, as php raises
    /// `EG(exception)` where the pointer answers `NULL`. Two extensions'
    /// hooks chain as php's saved pointer does: the later is served
    /// first and the earlier when the later serves the one beneath.
    #[must_use]
    pub fn compile_string(mut self, hook: CompileStringHook) -> ExtensionBuilder {
        self.supplies.compile_string = Some(hook);
        self
    }

    /// `zend_error_cb` overridden, as soap's `MINIT` overrides it with
    /// `soap_error_handler`: every diagnostic the user's error handler
    /// does not take, and every uncaught throwable rendered, is served
    /// by `hook`, handed the one beneath ([`Beneath`]), which ends
    /// in php's own `php_error_cb` that stores, logs and displays it.
    /// A hook that does not serve the one beneath has served the
    /// diagnostic itself; a fatal ends the request all the same. Two
    /// extensions' hooks chain as php's saved pointer does: the later is
    /// served first and the earlier when the later serves the one
    /// beneath.
    #[must_use]
    pub fn error(mut self, hook: ErrorHook) -> ExtensionBuilder {
        self.supplies.error = Some(hook);
        self
    }

    /// `zend_throw_exception_hook` set, as the tracers and profilers set
    /// it to see every throwable: `hook` is told of each throwable
    /// thrown, once, where it is thrown and before the unwind leaves the
    /// frame, with it standing pending: by `throw`, by a native, by
    /// `Generator::throw()`, and as a fiber hands one across a switch,
    /// in either direction, which php throws again
    /// (`zend_fiber_delegate_transfer_result`). As php's, it is told
    /// nothing of a throwable raised while another is pending, which is
    /// chained under it, nor of one raised where nothing executes. Two
    /// extensions' hooks chain as php's saved pointer does: the later is
    /// told first, handed the earlier ([`Beneath`]) to tell where it
    /// will, as each of php's links serves before the one it saved, and
    /// nothing of php's stands beneath them.
    #[must_use]
    pub fn thrown(mut self, hook: ThrownHook) -> ExtensionBuilder {
        self.supplies.thrown = Some(hook);
        self
    }

    /// An observer of user calls and natives,
    /// `zend_observer_fcall_register` at `MINIT` (design §4.3 seam 9):
    /// `init` is asked of each function, user or native, on its first
    /// call in a request, and of each included, evaluated or main file's
    /// frame as it is pushed, which handlers observe it; a begin handler
    /// runs as the frame is pushed, before its parameters are received,
    /// or before a native's entry, and an end handler as it is left,
    /// handed what it returned, or nothing where an unwind or a fatal
    /// left it or a native a call opcode served left a throwable
    /// standing. Every frame begun with an end handler is ended once.
    /// The bundle's observers are asked in the order the extensions
    /// start, and an extension may declare more than one; begin handlers
    /// are called in that order and end handlers in its reverse. A
    /// generator's frame is not begun at its call: it is begun at each
    /// resumption and ended at each suspension with what it yielded, and
    /// at its `return` with what it returned. With none declared, a call
    /// pays one load and one branch at the push and one at the return.
    #[must_use]
    pub fn observer(mut self, init: ObserverInit) -> ExtensionBuilder {
        self.supplies.observers.push((init, None));
        self
    }

    /// [`ExtensionBuilder::observer`] registered only where `registered`
    /// says so of the engine's start, as `zend_test`'s `MINIT` registers
    /// its observer under `zend_test.observer.enabled`: an observer not
    /// registered is never asked, and counts for none of what a declared
    /// observer changes, a native call's room among them.
    #[must_use]
    pub fn observer_when(
        mut self,
        init: ObserverInit,
        registered: ObserverRegistered,
    ) -> ExtensionBuilder {
        self.supplies.observers.push((init, Some(registered)));
        self
    }

    /// An observer of diagnostics, `zend_observer_error_register` at
    /// `MINIT` (design §4.3 seam 9): `observer` is told of every
    /// diagnostic `zend_error` raises, its level, file, line and
    /// message, before the user's error handler or the `zend_error_cb`
    /// chain ([`ExtensionBuilder::error`]) is handed it and whatever
    /// `error_reporting` says, and of every uncaught throwable rendered,
    /// as the `E_ERROR` it becomes; a diagnostic a compile records is told
    /// when it is raised after. It changes nothing of how the diagnostic
    /// is served: that is the `zend_error_cb` chain's. The bundle's are
    /// told in the order the extensions start, and an extension may
    /// declare more than one.
    #[must_use]
    pub fn error_observer(mut self, observer: ErrorObserver) -> ExtensionBuilder {
        self.supplies.error_observers.push(observer);
        self
    }

    /// An observer of declared functions,
    /// `zend_observer_function_declared_register` at `MINIT`: `observer`
    /// is told of each function a request declares, with the lowered name
    /// it is declared under, as a file's top-level functions are declared
    /// when it loads, before its first line runs, and as a
    /// `DECLARE_FUNCTION` declares one written inside a block. A file
    /// served from the engine's cache is told again on every load, as
    /// opcache tells php's observers on every hit.
    #[must_use]
    pub fn function_declared_observer(mut self, observer: FunctionDeclared) -> ExtensionBuilder {
        self.supplies.functions_declared.push(observer);
        self
    }

    /// An observer of linked classes,
    /// `zend_observer_class_linked_register` at `MINIT`: `observer` is
    /// told of each class a request links into its table, with the
    /// lowered name it stands under, as a file's classes bound early are
    /// linked when it loads, as a `DECLARE_CLASS` links one, and as
    /// `class_alias()` gives a class of the program another name. An
    /// anonymous class's declaration tells none, as php's
    /// `DECLARE_ANON_CLASS` does not, save one opcache's load linked,
    /// which it tells with the file's others; nor does an alias of an
    /// extension's class. A file served from the engine's cache is told
    /// again on every load.
    #[must_use]
    pub fn class_linked_observer(mut self, observer: ClassLinked) -> ExtensionBuilder {
        self.supplies.classes_linked.push(observer);
        self
    }

    /// Observation of a declared set of opcodes (design §7.1, as #1780
    /// ruled), php's `zend_set_user_opcode_handler` with a handler that
    /// answers `ZEND_USER_OPCODE_DISPATCH`: `declared` is asked once, as
    /// the engine starts, which opcodes the extension observes, handed
    /// what its `MINIT` is; `handler` is told of each opline of one of
    /// them the loop dispatches, before the opcode's handler runs, handed
    /// the frame standing at it and the one beneath ([`Beneath`]), which
    /// names the opcode as php names it. The opcode's handler always runs
    /// after it: no extension replaces one. Two extensions that declare
    /// one opcode chain as php's handler for it does: the later is told
    /// first, and the earlier where the later serves the one beneath, as
    /// xdebug's saves the handler it set over and calls it after its own
    /// work; nothing of php's stands beneath them. A second declaration
    /// of one extension stands over its first. With none declared, a
    /// serving reads one word as its loop is entered and each opline
    /// pays a test of it.
    #[must_use]
    pub fn opcodes(
        mut self,
        declared: OpcodesDeclared,
        handler: OpcodeHandler,
    ) -> ExtensionBuilder {
        self.supplies.opcodes = Some(crate::observer::OpcodeObserver { declared, handler });
        self
    }

    /// Told of each fiber's creation, `zend_observer_fiber_init_register`
    /// at `MINIT` (design §4.3 seam 9): `Fiber::start` has laid the
    /// fiber's stack down and has not yet switched into it. The bundle's
    /// handlers are called in the order the extensions start, an
    /// extension's in the order it declared them.
    #[must_use]
    pub fn fiber_init(mut self, handler: FiberHandler) -> ExtensionBuilder {
        self.supplies.fiber_init.push(handler);
        self
    }

    /// Told of each switch between fibers, the request's own stack
    /// among them, `zend_observer_fiber_switch_register`: handed the
    /// fiber left and the fiber entered, while the one left is still the
    /// one served. Every frame a fiber that ended in a fatal left open
    /// has been ended by then; each fiber's open frames are its own, so
    /// what is begun in a fiber is ended in it.
    #[must_use]
    pub fn fiber_switch(mut self, handler: FiberSwitchHandler) -> ExtensionBuilder {
        self.supplies.fiber_switch.push(handler);
        self
    }

    /// Told of each fiber given back once it is dead and its resumer is
    /// served again, `zend_observer_fiber_destroy_register`. A fiber
    /// never started was never created, and one still suspended when the
    /// request ends in a fatal is never told of.
    #[must_use]
    pub fn fiber_destroy(mut self, handler: FiberHandler) -> ExtensionBuilder {
        self.supplies.fiber_destroy.push(handler);
        self
    }

    /// The `gc_collect_cycles` pointer overridden, as xdebug's and the
    /// profilers' `MINIT` override it to measure each collection: every cycle
    /// collection, by `gc_collect_cycles()` or by a full root buffer, is
    /// served by `hook`, handed the one beneath ([`Beneath`]) to
    /// serve where its work is to stand; what it answers is the collection's
    /// count. Two extensions' hooks chain as php's saved pointer does:
    /// the later is served first and the earlier when the later serves
    /// the one beneath.
    #[must_use]
    pub fn gc_collect(mut self, hook: GcCollectHook) -> ExtensionBuilder {
        self.supplies.gc_collect = Some(hook);
        self
    }

    /// The one question the unit cache asks about a file (design §6.3,
    /// revision 45), what opcache's `persistent_compile_file` decides:
    /// `hook` is handed the file's full path, resolved, and answers
    /// whether the file is kept out of the cache and, where it is held,
    /// how a hit is found and validated ([`Admission`]). The last
    /// extension to install a hook answers, in bundle order and a Zend
    /// extension's after every other's, as opcache's
    /// `persistent_compile_file` stands over what it saved; with none,
    /// every file is held and validated on every hit.
    #[must_use]
    pub fn admission(mut self, hook: fn(&mut Request, &[u8]) -> Admission) -> ExtensionBuilder {
        self.supplies.admission = Some(hook);
        self
    }

    /// Asked once as the engine finishes starting up, on a request of
    /// the first host's that serves no program (design §6.3): php's
    /// `zend_post_startup_cb`, where opcache refuses a configuration it
    /// cannot start under and names the file `opcache.preload` serves.
    /// What the hook writes to the error stream is written before
    /// anything the engine serves. Two extensions' hooks chain as php's
    /// saved pointer does: the later is asked first, handed the earlier
    /// ([`Beneath`]) to ask where it will, as opcache's asks the one it
    /// saved before its own work and answers what it answered where that
    /// did not serve; beneath every link the chain serves.
    #[must_use]
    pub fn post_startup(mut self, hook: PostStartupHook) -> ExtensionBuilder {
        self.supplies.post_startup = Some(hook);
        self
    }

    /// Told of each file the unit cache compiles and holds, by its full
    /// path, on the request that compiled it (design §6.3): where php's
    /// `cache_script_in_shared_memory` has persisted a file, and opcache
    /// logs `Cached script` at `ACCEL_LOG_INFO`. Every extension with a
    /// hook is told, in bundle order and a Zend extension after every
    /// other.
    #[must_use]
    pub fn cached(mut self, hook: fn(&mut Request, &[u8])) -> ExtensionBuilder {
        self.supplies.cached = Some(hook);
        self
    }

    /// `zend_optimizer_register_pass`: `pass` is called with each file
    /// opcache optimizes, by its full path, on the request that compiled
    /// it, where `zend_optimize_script` calls the registered passes after
    /// its own (`zend_optimizer_call_registered_passes`): a file the
    /// unit cache compiles and holds, or keeps in the file cache alone,
    /// and each file a preload saves. Every extension's passes are called
    /// in the order they were registered, an extension's in the order it
    /// hands them here. This engine has no optimizer over an executed op
    /// array (design §6.1, revision 44), so no op array crosses the
    /// face: a pass is told the file, and rewrites nothing.
    #[must_use]
    pub fn optimizer_pass(mut self, pass: fn(&mut Request, &[u8])) -> ExtensionBuilder {
        self.supplies.optimizer_passes.push(pass);
        self
    }

    /// The host's log for a fatal, `sapi_module.log_message` as phpdbg's
    /// `php_sapi_phpdbg_log_message` serves it (`Z/sapi/phpdbg/phpdbg.c:731-770`):
    /// asked once the host's log has taken a diagnostic of a fatal level,
    /// with the request standing where the fatal was raised and before
    /// `php_error_cb` displays it, writes the status and bails out, so
    /// what the hook serves runs on the frames the fatal left standing.
    /// The host's table has one slot, so the last extension with a hook
    /// is the log, and the host's own extension starts last. An
    /// extension a host declares for itself is the one that hands it
    /// (ADR-0008, #1348).
    #[must_use]
    pub fn fatal_logged(mut self, hook: fn(&mut Request) -> AfterLog) -> ExtensionBuilder {
        self.supplies.fatal_logged = Some(hook);
        self
    }

    /// Served once the primary program has ended and before the request
    /// shuts down, the moment between `php_execute_script` and
    /// `php_request_shutdown` where php-fpm's loop puts the line
    /// `fastcgi.error_header` names into the head
    /// (`Z/sapi/fpm/fpm/fpm_main.c:1931-1940`): every extension's hook in
    /// bundle order, for a program served whole and not for a lint, a
    /// highlight or a strip. An extension a host declares for itself is
    /// the one that hands it (ADR-0008, #1348).
    #[must_use]
    pub fn program_ended(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.supplies.program_ended = Some(hook);
        self
    }

    /// The CLI's interactive shell ([`Shell`]): what `readline_cli.c`'s
    /// `MINIT` writes into the table of a host that carries one. The
    /// last extension in bundle order with a shell is the one served,
    /// as the last `MINIT` to write the table wins.
    #[must_use]
    pub fn shell(mut self, shell: Shell) -> ExtensionBuilder {
        self.supplies.shell = Some(shell);
        self
    }

    /// The input filter, php's `sapi_module.input_filter` as `ext/filter`'s
    /// `MINIT` registers `php_sapi_filter` (`sapi_register_input_filter`):
    /// each variable php registers from what a host described, a query
    /// string's, a cookie's, a form body's, a `$_SERVER` row the host
    /// registers, and `parse_str()`'s, is handed to `hook` with where it
    /// came from, its name and its value, before it is registered; the
    /// value `hook` answers is registered in its place, and `None` drops
    /// the variable. The last extension in bundle order with a hook is the
    /// filter; with none, every value is registered as it came.
    #[must_use]
    pub fn input_filter(mut self, hook: InputFilterHook) -> ExtensionBuilder {
        self.supplies.input_filter = Some(hook);
        self
    }

    /// The data treater, php's `sapi_module.treat_data` as mbstring's
    /// `MINIT` registers `mbstr_treat_data` (`sapi_register_treat_data`),
    /// with the form body's post handler it registers beside it: a query
    /// string's, a cookie header's, a form body's and `parse_str()`'s
    /// source is handed to `hook` first, which answers the variables it
    /// read, each name and value as they are to be registered; the
    /// engine hands each to the input filter and registers what the
    /// filter keeps, as `_php_mb_encoding_handler_ex` does. `None` leaves
    /// the source to `php_default_treat_data`. The last extension in
    /// bundle order with a hook treats the data.
    #[must_use]
    pub fn treat_data(mut self, hook: TreatDataHook) -> ExtensionBuilder {
        self.supplies.treat_data = Some(hook);
        self
    }

    /// `zend_resolve_path` overridden, as phar's `MINIT` overrides it with
    /// `phar_resolve_path`: a path an include or an open along the
    /// `include_path` names is handed to `hook` first, which may answer
    /// where it stands, a `phar://` URL inside the archive being served,
    /// or serve the one beneath ([`Beneath`]), which ends in the engine's
    /// own resolution. Two extensions' hooks chain as php's saved pointer
    /// does: the later is asked first, and the earlier where the later
    /// serves the one beneath.
    #[must_use]
    pub fn resolve_path(mut self, hook: ResolvePathHook) -> ExtensionBuilder {
        self.supplies.resolve_path = Some(hook);
        self
    }

    /// The stream layer's hooks ([`crate::layer::StreamHooks`]), which `ext/standard`
    /// hands as php's `basic_functions` registers the plain files wrapper
    /// in its `MINIT`. The last supplier's are the ones asked, as
    /// `php_module_startup` writes `zend_stream_open_function` once.
    #[must_use]
    pub fn stream_hooks(mut self, hooks: crate::layer::StreamHooks) -> ExtensionBuilder {
        self.supplies.stream_hooks = Some(hooks);
        self
    }

    /// The encoding functions, php's `zend_multibyte_functions` as
    /// mbstring's `MINIT` hands them to `zend_multibyte_set_functions`:
    /// `bind` answers them for one request as its settings stand, the
    /// internal encoding, the detect order and the substitute character,
    /// and the engine asks it each time it opens a file under
    /// `zend.multibyte`, each time `zend.script_encoding` is set, and as it
    /// reads a multipart form body ([`Multibyte`]). The last extension in bundle order that hands
    /// them is the provider, as a later `zend_multibyte_set_functions`
    /// replaces an earlier one; with none, Zend's dummies answer, under
    /// which nothing is fetched or converted.
    #[must_use]
    pub fn encodings(mut self, bind: EncodingsHook) -> ExtensionBuilder {
        self.supplies.encodings = Some(bind);
        self
    }

    /// The upload hook, php's `php_rfc1867_callback` as `ext/session`'s
    /// `MINIT` sets it for `session.upload_progress`: each step of a
    /// multipart body's reading is handed to `hook` as it is taken
    /// ([`crate::api::request::Upload`]), and `false` answers php's
    /// `FAILURE`: the body's reading stops at its start, a field is not
    /// registered, a file is passed over at its start and cancelled with
    /// `UPLOAD_ERR_EXTENSION` after it. Two extensions' hooks chain as
    /// php's saved pointer does: the later is asked first, handed the
    /// earlier ([`Beneath`]) to ask where it will, as session's asks the
    /// one it saved before its own work and keeps its answer; beneath
    /// every link the chain answers `true`, php's `SUCCESS`.
    #[must_use]
    pub fn upload(mut self, hook: UploadHook) -> ExtensionBuilder {
        self.supplies.upload = Some(hook);
        self
    }

    /// The extension is a Zend extension too, php's `zend_extension`
    /// beside its `zend_module_entry` (`Z/Zend/zend_extensions.c`), as
    /// opcache is: `row` is what `php -m` lists under `[Zend Modules]`,
    /// `get_loaded_extensions(true)` answers and `ReflectionZendExtension`
    /// reads.
    #[must_use]
    pub fn zend_extension(mut self, row: ZendExtension) -> ExtensionBuilder {
        self.zend = Some(row);
        self
    }

    /// A native another extension declares, its entry replaced by `entry`
    /// as the engine is built, as phar's `MINIT` replaces `fopen`,
    /// `file_get_contents`, `stat` and their kin in the function table
    /// (`phar_intercept_functions_init`): the declaration stays the
    /// native's, what a call serves is `entry`, and the entry it
    /// replaced is kept for [`crate::api::call::passthrough`]. Two
    /// extensions that intercept one native stack in the order php
    /// installs them, the later over the earlier: each extension's at
    /// its `MINIT` in bundle order, then each Zend extension's, so
    /// opcache's handler is saved over phar's whatever their bundle
    /// order. A name no extension declares, or one `disable_functions`
    /// took out, is intercepted by nothing.
    #[must_use]
    pub fn intercept(mut self, name: &[u8], entry: Entry) -> ExtensionBuilder {
        self.supplies.intercepts.push((Name(name.into()), entry));
        self
    }

    /// A value this extension hands at startup to the extension
    /// that reads every value of its type, php's `php_pdo_register_driver`
    /// into `pdo_driver_hash` from `pdo_sqlite`'s `MINIT`: immutable once the
    /// engine is built, and answered by [`Request::registered`] in bundle
    /// order, which is `MINIT` order (ADR-0005).
    #[must_use]
    pub fn register<T: Send + Sync + 'static>(mut self, value: T) -> ExtensionBuilder {
        self.supplies
            .registered
            .push((TypeId::of::<T>(), Arc::new(value)));
        self
    }

    /// This extension's constants as the engine links them: what
    /// [`ExtensionBuilder::constants`] declared, handed to `link` with
    /// every value of type `T` the bundle registered
    /// ([`ExtensionBuilder::register`]), in bundle order, and what `link`
    /// answers is the extension's list. php's `php_image_register_handler`,
    /// which another extension's `MINIT` calls into `standard` to add a
    /// constant to `standard`'s own group and raise its `IMAGETYPE_COUNT`,
    /// is this rewrite done once, when the engine is built.
    #[must_use]
    pub fn constants_with<T: Send + Sync + 'static>(
        mut self,
        link: fn(Vec<ConstDecl>, &[&T]) -> Vec<ConstDecl>,
    ) -> ExtensionBuilder {
        self.constants_link = Some(Arc::new(move |declared, registered| {
            let values: Vec<&T> = registered
                .under(TypeId::of::<T>())
                .filter_map(|(_, value)| value.downcast_ref::<T>())
                .collect();
            link(declared, &values)
        }));
        self
    }

    /// Constants this extension declares on a class another extension
    /// declares, php's `zend_declare_class_constant_ex` over a foreign
    /// `zend_class_entry` at `MINIT`, which is how `pdo_sqlite` puts its
    /// deprecated `PDO::SQLITE_*` aliases on `PDO`: appended to that
    /// class's own as the engine links it, after every constant the class
    /// declares, in bundle order. A class the bundle does not declare
    /// takes none.
    #[must_use]
    pub fn class_constants(
        mut self,
        class: &[u8],
        constants: Vec<ClassConstDecl>,
    ) -> ExtensionBuilder {
        self.class_constants.push((Name(class.into()), constants));
        self
    }

    /// Methods of a class another extension declares that the class's
    /// `get_method` answers and its table carries none of, `pdo`'s
    /// methods (`pdo_dbh_methods.get_driver_methods`, which
    /// `pdo_hash_methods` lays down with `func.scope = pdo_dbh_ce`): each
    /// is a trampoline of `class` ([`Request::trampoline`]), so no lookup
    /// by name and no rendering finds it, and its diagnostics name the
    /// class it is laid down on.
    #[must_use]
    pub fn trampolines(
        mut self,
        class: &[u8],
        methods: Vec<(MethodDecl, Entry)>,
    ) -> ExtensionBuilder {
        self.trampolines.extend(
            methods
                .into_iter()
                .map(|(method, entry)| (Name(class.into()), method, entry)),
        );
        self
    }

    /// The extension's own version, `zend_module_entry.version`, as dom's
    /// `DOM_API_VERSION` is `20031129`: what `phpversion('dom')`,
    /// `ReflectionExtension::getVersion()` and its rendering print. An
    /// extension that declares none has `PHP_VERSION`, as
    /// `PHP_CTYPE_VERSION` and its kin are.
    #[must_use]
    pub fn version(mut self, version: &'static str) -> ExtensionBuilder {
        self.version = Some(version);
        self
    }

    /// The extension, named as php names it (`ctype`, `pcre`).
    #[must_use]
    pub fn build(self, name: &str) -> Extension {
        Extension {
            entry: ExtensionEntry {
                name: name.to_owned(),
                listed: true,
                version: self.version.unwrap_or(crate::api::request::PHP_VERSION),
                ini: self.ini,
                on_modify: self.on_modify,
                dependencies: self.dependencies,
                request_state: self.request_state,
                process_state: self.process_state,
                engine_state: self.engine_state,
                rinit: self.rinit,
                rshutdown: self.rshutdown,
                deactivate: self.deactivate,
                post_deactivate: self.post_deactivate,
                engine_end: self.engine_end,
                supplies: Supplies {
                    statement_handler: self.zend.as_ref().and_then(|row| row.statement_handler),
                    ..self.supplies
                },
                zend: self.zend,
                info: self.info,
            },
            declarations: Declarations {
                functions: self.functions,
                classes: self.classes,
                constants: self.constants,
                constants_link: self.constants_link,
                class_constants: self.class_constants,
                trampolines: self.trampolines,
            },
        }
    }
}

/// A property as `zend_declare_typed_property` declares one on an
/// internal class (`Z/Zend/zend_API.c:4517`): a public readonly property
/// with no set visibility of its own is `protected(set)`.
fn declared_property(mut property: PropertyDecl) -> PropertyDecl {
    if property.is_readonly
        && property.visibility == Visibility::Public
        && property.set_visibility.is_none()
    {
        property.set_visibility = Some(Visibility::Protected);
    }
    property
}

/// A validated callable: a function name, a `[$object, 'method']` pair,
/// a `Class::method` string or a `Closure`, as `zend_is_callable` accepts
/// them, held with the value it came from and the callee it resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct Callable {
    value: Value,
    name: String,
    target: Target,
}

impl Callable {
    pub(crate) fn of(value: Value, name: &[u8], target: Target) -> Callable {
        Callable {
            value,
            name: String::from_utf8_lossy(name).into_owned(),
            target,
        }
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    /// The callable moved out of `self`, which keeps its value and target
    /// and an empty name: what an owner of it is made of as the owner is
    /// consumed, the name's allocation moved with it.
    pub(crate) fn moved_out(&mut self) -> Callable {
        Callable {
            value: self.value,
            name: std::mem::take(&mut self.name),
            target: self.target,
        }
    }

    /// `zend_fcall_info_cache` as `zend_is_callable_ex` filled it: what
    /// the callable resolved to when it was made, which is what serving
    /// it enters. php holds the same three fields for the same span,
    /// `zend_fcc_dup` past the frame the callable was read off.
    pub(crate) const fn target(&self) -> Target {
        self.target
    }

    /// The value the callable came from, a view and no owner taken. A
    /// list that holds the callable past the call frame it was read off
    /// holds an [`Owner`] of the callable, not of this alone.
    #[must_use]
    pub const fn value(&self) -> Value {
        self.value
    }

    /// `zend_get_callable_zval_from_fcc` (`Z/Zend/zend_API.c:4447`): the
    /// value php makes again of what the callable resolved to, not the
    /// value it came from. A closure is itself; a method is the pair of
    /// its receiver, or of its scope's name where it has none, and its
    /// name; a function is its name. So a `Class::method` string resolved
    /// with `$this` in scope is made again as `[$this, 'method']`, which
    /// resolves in any scope, as `RecursiveCallbackFilterIterator`'s
    /// children are made with it:
    ///
    /// ```php
    /// class A { function __destruct(){ echo "dtor\n"; } function f($v, $k, $it){ echo "f $k\n"; return true; }
    ///   function make(){ return new RecursiveIteratorIterator(new RecursiveCallbackFilterIterator(new RecursiveArrayIterator([1, [2, 3]]), 'A::f')); } }
    /// $it = (new A)->make();
    /// foreach ($it as $v) echo $v, "\n";
    /// unset($it);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// f 0
    /// 1
    /// f 1
    /// f 0
    /// 2
    /// f 1
    /// 3
    /// dtor
    /// end
    /// ```
    pub fn made_again(&self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        let (function, first) = match self.target {
            Target::Closure(closure) => return Ok(Owner::take(req, Value::object(closure))),
            Target::Function(function) => (function, None),
            Target::Method {
                function,
                scope,
                this,
            } => (function, Some(this.ok_or(scope))),
        };
        let name = req
            .function_decl(function)
            .map(|decl| decl.name.0.clone())
            .unwrap_or_default();
        let name = PhpString::declared(req, &name)?.into_value();
        let Some(first) = first else {
            return Ok(name);
        };
        let first = match first {
            Ok(this) => Owner::take(req, this.as_value()),
            Err(scope) => {
                let class = req
                    .class_decl(scope)
                    .map(|decl| decl.name.0.clone())
                    .unwrap_or_default();
                match PhpString::declared(req, &class) {
                    Ok(class) => class.into_value(),
                    Err(thrown) => {
                        name.give_up(req);
                        return Err(thrown);
                    }
                }
            }
        };
        let mut pair = match PhpArray::new(req) {
            Ok(pair) => pair,
            Err(thrown) => {
                first.give_up(req);
                name.give_up(req);
                return Err(thrown);
            }
        };
        let mut values = [first, name].into_iter();
        while let Some(value) = values.next() {
            if let Err(thrown) = pair.append(req, value) {
                values.for_each(|value| value.give_up(req));
                pair.give_up(req);
                return Err(thrown);
            }
        }
        Ok(pair.into_value())
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }

    /// `fci_cache.called_scope = called_scope` where
    /// `instanceof_function(called_scope, fci_cache.calling_scope)`: a
    /// method the callable resolved to is served with `called` as its
    /// called scope where `called` is its class or a class that extends it,
    /// the called scope `forward_static_call` forwards; a
    /// function, a closure and a method of an unrelated class are
    /// served as they were resolved. The value and the receiver are the
    /// same, so the owners are the callable's.
    ///
    /// ```php
    /// class A { public static function who() { return static::class; } }
    /// class B extends A { public static function test() { return [forward_static_call(['A', 'who']), call_user_func(['A', 'who']), forward_static_call('strtoupper', 'x')]; } }
    /// class C { public static function test() { return forward_static_call(['A', 'who']); } }
    /// var_dump(B::test(), C::test());
    /// ```
    /// ```expect
    /// array(3) {
    ///   [0]=>
    ///   string(1) "B"
    ///   [1]=>
    ///   string(1) "A"
    ///   [2]=>
    ///   string(1) "X"
    /// }
    /// string(1) "A"
    /// ```
    #[must_use]
    pub fn with_called_scope(&self, req: &Request, called: ClassId) -> Callable {
        let mut forwarded = self.clone();
        if let Target::Method { scope, .. } = &mut forwarded.target
            && req.instance_of(called, *scope)
        {
            *scope = called;
        }
        forwarded
    }

    /// `fcc.function_handler->common.required_num_args`: the parameters
    /// of the callee up to the last one a call must pass, what
    /// `zend_ffi_create_callback` holds a C function's arity to.
    ///
    /// ```php
    /// $ffi = FFI::cdef("typedef struct { int (*cb)(int); } s;");
    /// $s = $ffi->new("s");
    /// try { $s->cb = function ($a, $b) { return 0; }; } catch (FFI\Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Attempt to assign an invalid callback, insufficient number of arguments
    /// ```
    #[must_use]
    pub fn required(&self, req: &Request) -> usize {
        let function = match self.target {
            Target::Function(function) | Target::Method { function, .. } => function,
            Target::Closure(closure) => match closure.closure(req) {
                Some(info) => info.function,
                None => return 0,
            },
        };
        req.function_decl(function).map_or(0, |decl| {
            decl.parameters
                .iter()
                .rposition(php_unit::decl::Parameter::is_required)
                .map_or(0, |at| at + 1)
        })
    }
}

/// What `has_property` is asked of a property, php's
/// `ZEND_PROPERTY_ISSET`, `ZEND_PROPERTY_NOT_EMPTY` and
/// `ZEND_PROPERTY_EXISTS`: `isset($o->p)`, `empty($o->p)` and
/// `property_exists`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyTest {
    Isset,
    NotEmpty,
    Exists,
}

/// Which read `read_property` is serving, php's `type` argument to the
/// slot: `BP_VAR_R`, the read that warns of a property nothing holds
/// and refuses one the scope may not reach; `BP_VAR_IS`, the read an
/// `isset`, an `empty` or a `??` makes, which raises none of the three
/// and answers `null`; and `BP_VAR_W`, the read a write fetch falls
/// back to where `get_property_ptr_ptr` answered no place, which reads
/// as `BP_VAR_R` does and refuses a hooked property's value that no
/// write through it could reach back into; and `BP_VAR_UNSET`, the read
/// an `unset` through the property falls back to, which answers `null`
/// for what nothing holds and asks no `__isset`. Under `BP_VAR_W` and
/// `BP_VAR_UNSET` a value `__get` answers that is neither an object nor a
/// reference is php's notice that the modification has no effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyRead {
    Warning,
    Silent,
    Write,
    Unset,
}

/// Which fetch `read_dimension` is serving, php's `type` argument to
/// the slot: `BP_VAR_R`, the read that warns of a key the container has
/// not; `BP_VAR_IS`, the read an `isset`, an `empty` or a `??` makes,
/// which warns of nothing; and the three fetches a write through the
/// element makes, `BP_VAR_W` (`$o[$k][] = v`, `$o[$k]->p = v`, `&$o[$k]`),
/// `BP_VAR_RW` (`$o[$k][$j] .= v`, `$o[$k]++` over an array element of
/// it) and `BP_VAR_UNSET` (`unset($o[$k][$j])`), under which a class
/// whose elements are places hands the element back as a reference,
/// the place itself, as `spl_array_read_dimension_ex` makes it one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DimensionRead {
    Warning,
    Silent,
    Write,
    ReadWrite,
    Unset,
}

/// What `get_property_ptr_ptr` answers for one name, php's pointer or
/// its `NULL`: whether a write to the property addresses the standard
/// place, or goes through the class's `read_property` and
/// `write_property` because the class serves the name itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyPlace {
    /// `zend_std_get_property_ptr_ptr`'s place: the declared slot or
    /// the dynamic entry, which the kernel lays down itself under the
    /// scope's access rule, with the `__get` arm and every diagnostic
    /// the standard slot raises.
    Std,
    /// php's `NULL`: the class serves the name, so a write reaches its
    /// `write_property`, and a compound write, an increment and a write
    /// fetch read through its `read_property` first.
    Handlers,
    /// php's pointer to a value the class keeps apart from its
    /// properties (`sxe_property_get_adr`'s `&sxe->tmp`): a write fetch,
    /// a compound write and an increment read it through `read_property`
    /// under [`PropertyRead::Write`], which lays it down, and change it
    /// in place, nothing written back; a plain write, which php's
    /// `ASSIGN_OBJ` makes without asking, reaches `write_property`.
    Apart,
}

/// What `get_properties_for` is asked for, php's `zend_prop_purpose`:
/// the six readers of an object's properties that a class may answer
/// each in its own way, `GetObjectVars` being `get_object_vars()`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyPurpose {
    Debug,
    ArrayCast,
    Serialize,
    VarExport,
    Json,
    GetObjectVars,
}

/// What `get_properties_for` answers for one purpose, php's
/// `HashTable *` and its `*is_temp`: a table the object or the class
/// keeps, which the reader holds one more owner of while it reads
/// (`GC_TRY_ADDREF`); a table built for the reader, its one owner the
/// reader's to give up (`zend_release_properties`); or php's `NULL`
/// table, which no reader may stand in for with an empty one, since
/// `get_object_vars()`, the `(array)` cast and
/// `get_mangled_object_vars()` answer the process's empty array for it
/// (`ZVAL_EMPTY_ARRAY`, `Z/Zend/zend_builtin_functions.c:849`,
/// `Z/Zend/zend_execute.h:280`) where a table, however empty, is copied
/// into an array of the request's.
#[derive(Debug)]
pub enum PropertiesFor {
    /// A table the object or the class keeps, a view: what
    /// `get_properties` answers, or a table the class holds in its
    /// place (`spl_array_get_hash_table`'s storage).
    Table(walk::Table),
    /// A table the class built for this reader (`*is_temp`), each row
    /// under the key php's table holds it by.
    Temporary(Owner<PhpArray>),
    /// Under [`PropertyPurpose::Debug`], the table the class's
    /// `get_debug_info` built in place of its properties, a temporary
    /// one, with what it left pending while it built it.
    Debug(DebugInfo),
    /// The slot's `NULL`: no table, which a dumper, an encoder and a
    /// serializer read as no properties
    /// (`attributes_sensitive_parameter_value_get_properties_for`).
    Null,
    /// `zend_hooked_object_build_properties`' table, built for the
    /// reader of an object whose class has a hooked property, whose
    /// hooked rows the reader reads as it reaches them
    /// ([`crate::api::values::walk::Row::read`]). Only the engine's
    /// `Std` answers it.
    Hooked(HookedTable),
}

/// What the `get_debug_info` slot answers: php's `HashTable *` with its
/// `*is_temp` owner, the caller's, which the caller gives up once the
/// dump is printed, and the throwable a handler raised
/// while it built the table and left in `EG(exception)`, which
/// `dom_get_debug_info_helper` does for a property whose reader fails
/// (`Z/ext/dom/php_dom.c:526`). A dumper renders the table and then
/// hands the throwable on, so the dump is printed and the throwable is
/// thrown once the dumper returns, as `php_var_dump` leaves it
/// (`Z/ext/standard/var.c:184`). A second throwable the handler raises
/// is chained onto the first with [`Thrown::chain`].
#[derive(Debug)]
pub struct DebugInfo {
    pub table: Owner<PhpArray>,
    pub thrown: Option<Thrown>,
}

impl From<Owner<PhpArray>> for DebugInfo {
    fn from(table: Owner<PhpArray>) -> DebugInfo {
        DebugInfo {
            table,
            thrown: None,
        }
    }
}

/// The table php's `get_gc` returns beside the values it names, which
/// the cycle collector colours and walks after them (`Z/Zend/zend_gc.c:1112`)
/// in place of the object's properties, so the order its garbage is
/// found in, and its destructors served in, is php's.
pub enum GcTable {
    /// `zend_std_get_properties(obj)`, or php's `NULL`: the object's
    /// declared slots and dynamic table, which the heap walks itself.
    Properties,
    /// A table the payload owns and frees, as `zend_closure_get_gc`
    /// returns the closure's static variables.
    Owned(PhpArray),
    /// A table that is no value, the symbol table a frame rebuilt, which
    /// `zend_fiber_object_gc` and `zend_generator_get_gc` return: what
    /// its rows hold, in order.
    Rows(Vec<Value>),
}

/// What the `dtor_obj` slot holds: `zend_objects_destroy_object`, or a
/// destructor of the table's own. The difference is php's own test,
/// `dtor_obj != zend_objects_destroy_object || ce->destructor`, which
/// `zend_objects_store_del`, the cycle collector and a lazy object's reset
/// read: an object whose table serves its own is one with a destructor
/// whatever its class declares, so the collector serves it before it
/// frees anything and takes what it reaches out of the count
/// (`GC_DESTRUCTOR_GARBAGE`, `Z/Zend/zend_gc.c`).
#[derive(Clone, Copy)]
pub enum DtorObj {
    /// `zend_objects_destroy_object`: the class's `__destruct`, where it
    /// declares or inherits one, served on the object, which the store
    /// holds at one owner meanwhile.
    Destroy,
    /// The table's own, as `Fiber`'s `zend_fiber_object_destroy` and
    /// `IntlIterator`'s `IntlIterator_objects_dtor` are; one that serves
    /// the class's `__destruct` as well serves [`DtorObj::Destroy`] itself.
    Own(fn(&mut Request, PhpObject)),
}

impl DtorObj {
    /// The slot served on `object`.
    pub fn serve(self, req: &mut Request, object: PhpObject) {
        match self {
            DtorObj::Destroy => req.destruct(object),
            DtorObj::Own(own) => req.serve_as_table(object.class(), |req| own(req, object)),
        }
    }
}

/// The `clone_obj` slot: a fresh object holding what this one holds,
/// its one owner the caller's.
pub type CloneObj = fn(&mut Request, PhpObject) -> Result<Owner<PhpObject>, Thrown>;
/// The `clone_obj_with` slot: `clone($o, [...])`, the clone with each
/// named property of the table, php's `const HashTable *`, a view,
/// written through `write_property` from `scope`; the clone's one owner
/// the caller's.
pub type CloneObjWith =
    fn(&mut Request, PhpObject, Option<ClassId>, PhpArray) -> Result<Owner<PhpObject>, Thrown>;
/// What [`ObjectHandlers::iterator_element`] answers for one element: its
/// key and its value, one owner each, the caller's, as php's
/// `get_current_key` and `get_current_data` hand them to `zend_iterator`'s
/// reader.
pub type IteratorElement = (Owner<Value>, Owner<Value>);
/// The `current_place` slot: the element the object's own iterator
/// stands at made a reference in place, the cell with one more owner,
/// the caller's; `None` past the last element.
pub type CurrentPlace = fn(&mut Request, PhpObject) -> Result<Option<Owner<PhpRef>>, Thrown>;

/// The `zend_object_iterator` a class hands out from its own
/// `get_iterator` and walks in C, as [`ObjectHandlers::get_iterator`]
/// answers it: `iter->funcs` over the state the iterator keeps apart
/// from the object it walks, handed that object at every step, as
/// `spl_dllist_get_iterator`'s keeps a traverse pointer of its own, so
/// two loops over one list never move each other. Each step is served
/// in no frame of its own, as php's functions are, so a method one of
/// them calls is called from the loop's frame. The state names no value
/// of the heap; what it walks is the object's.
pub trait ClassIterator: Send {
    /// The rest of the class's `get_iterator` once `zend_iterator_init`
    /// has put the iterator in the object store, so what it makes takes a
    /// number after the iterator's: `pdo_stmt_iter_get` fetches its first
    /// row here. A throw is a throw from `get_iterator`, the iterator
    /// released through [`ClassIterator::release`] as
    /// `zend_iterator_dtor` releases it. Nothing by default.
    fn made(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let _ = (req, object);
        Ok(())
    }
    /// `funcs->rewind`.
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
    /// `funcs->valid`, `SUCCESS` as `true`.
    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown>;
    /// `funcs->get_current_data`, one owner, the caller's.
    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown>;
    /// `funcs->get_current_key`, one owner, the caller's, as php's
    /// caller frees the `zval` the function filled.
    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown>;
    /// `funcs->move_forward`.
    fn move_forward(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
    /// `funcs->dtor`, when the loop gives the iterator up, before the
    /// object it walked is: `spl_recursive_it_dtor` cuts the walk back
    /// to its root. Nothing by default.
    fn release(&mut self, req: &mut Request, object: PhpObject) {
        let _ = (req, object);
    }
}

/// Seam 2 (design §4.3, §5.4): Zend's 25 handler slots in 8.5 order
/// (`Z/Zend/zend_object_handlers.h:206-235`), one method each, the
/// default bodies being [`Std`]'s, the user-class semantics, so an
/// internal class overrides the slots it needs and inherits the rest.
/// The table is reached from an object through its class; the kernels
/// call the table, the methods here call the kernels, and nothing else
/// knows how an `ArrayObject` indexes.
///
/// A slot php marks required is a method with a default body. A slot
/// php marks optional answers `None` for php's null: `clone_obj` and
/// `clone_obj_with` answer the function the slot holds, since `clone`
/// reads the null before it looks at `__clone` and before it calls
/// anything; `count_elements`, `get_debug_info`, `get_closure` and
/// `do_operation` answer the value or `None` in one call, since
/// their kernels fall back either way; `get_properties_for` answers a
/// [`PropertiesFor`], whose slot may answer php's `NULL` table besides.
///
/// What a table refuses of construction, cloning and comparison it
/// states once, as [`ObjectHandlers::refusals`], and the default
/// `get_constructor`, `clone_obj` and `compare` serve it.
/// Which of Core's own handler tables a table is, the identity a kernel
/// asks where php compares a class or a handler to Core's
/// (`zend_ce_generator`, `get_gc == zend_weakmap_get_gc`, the
/// throwables' `zend_default_exception_new`, `Closure`'s
/// `zend_closure_new`): answered by the table, never read off its
/// address, which a constant the linker merges with Core's static would
/// share. An extension's table can name only [`CoreTable::NONE`]; the
/// others are the engine's to answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoreTable(Core);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Core {
    None,
    Throwable,
    Closure,
    Generator,
    WeakMap,
}

impl CoreTable {
    /// No table of Core's: what every table an extension declares is.
    pub const NONE: CoreTable = CoreTable(Core::None);

    /// The table Core's `kind` is.
    pub(crate) const fn of(kind: Core) -> CoreTable {
        CoreTable(kind)
    }

    /// Whether this is Core's `kind`.
    pub(crate) fn is(self, kind: Core) -> bool {
        self.0 == kind
    }
}

pub trait ObjectHandlers: Send + Sync {
    /// Which of Core's tables this is ([`CoreTable`]); none at `Std`
    /// and for every table an extension declares.
    fn core_table(&self) -> CoreTable {
        CoreTable::NONE
    }

    /// The class's [`Refusals`], php's `get_constructor` that throws,
    /// null `clone_obj` and `zend_objects_not_comparable`, which the
    /// default bodies of those three slots read; none at `Std`. A table
    /// that overrides one of the three slots answers it in its own words
    /// and the refusal it states for that slot is not read. Each default
    /// body is compiled per table, so the read is a call on the table's
    /// own type, never through the class's handlers pointer, and `Std`'s
    /// is the constant [`Refusals::NONE`].
    fn refusals(&self) -> Refusals {
        Refusals::NONE
    }

    /// `ce->create_object` past `zend_object_alloc`: what the class lays
    /// down with a new object on the request's heap that its payload's
    /// `Default` cannot, `array_init(&intern->array)` among them, after
    /// the properties' defaults and before any constructor. `Std` lays
    /// nothing down. On a class under `Throwable` it runs after
    /// `zend_default_exception_new` has written `file`, `line` and
    /// `trace`, as php's `ce->create_object` wraps
    /// `zend_ce_exception->create_object`, and the table answers
    /// `clone_obj` with `None`, as the throwables' does.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let _ = (req, object);
        Ok(())
    }

    /// `free_obj` for the native payload: what the payload names of the
    /// heap given back before the payload is dropped, with the object's
    /// bucket already invalid and its members still standing, so PHP
    /// code the release serves reads the object whole, as a subclass's
    /// `toUCallback` reads it under `ucnv_close`. `Std`'s body is
    /// `zend_object_std_dtor`: the weak references told and the members
    /// freed. The members are freed after the slot returns, as php's
    /// handlers call `zend_object_std_dtor` last; a table whose php
    /// handler calls it first, as `spl_array_object_free_storage` does,
    /// calls `Std.free_obj` where php's does, and nothing is left to free
    /// after. What PHP code the release serves throws is left pending
    /// with [`crate::api::raising::leave_pending`], for the release that
    /// freed the object to raise.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        req.free_members(object);
    }

    /// `dtor_obj`, [`DtorObj::Destroy`] at `Std`: what the store serves
    /// before it frees the object, the destructor phase of a collection
    /// before it frees the garbage, and a lazy object's reset before it
    /// resets.
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Destroy
    }

    /// `clone_obj`, `zend_objects_clone_obj` at `Std`: the members
    /// copied with one more owner each and `__clone` served on the copy.
    /// `None` is php's null slot, an uncloneable class, as a table that
    /// refuses cloning in its [`Refusals`] answers.
    fn clone_obj(&self) -> Option<CloneObj> {
        if self.refusals().cloning {
            return None;
        }
        Some(Request::clone_members)
    }

    /// `clone_obj_with`, `zend_objects_clone_obj_with` at `Std`: the
    /// class's `clone_obj`, then each given property written through
    /// `write_property`. `None` is php's null slot, a class whose
    /// objects `clone($o, [...])` refuses.
    fn clone_obj_with(&self) -> Option<CloneObjWith> {
        Some(Request::clone_members_with)
    }

    /// `read_property`, `zend_std_read_property` at `Std`: one more
    /// owner of the value read, a reference followed, the caller's, as
    /// php's caller frees the `rv` the slot read into. `read` is php's
    /// `type` argument, `BP_VAR_R` or the `BP_VAR_IS` an `isset`, an
    /// `empty` or a `??` reads through.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        req.read_property(object, name, read)
    }

    /// `write_property`, `zend_std_write_property` at `Std`: a sink of
    /// both owners, the value's the object's from here and the name's
    /// given up once the object has taken its own. A table that refuses
    /// the write gives both up.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.write_property(object, name, value)
    }

    /// `read_dimension`, `zend_std_read_dimension` at `Std`:
    /// `offsetGet` on an `ArrayAccess`, `null` for `$o[]`, and php's
    /// `Error` for an object of any other class; under
    /// [`DimensionRead::Silent`] `offsetExists` first, and `null` where
    /// it says no. `read` is php's `type`: under a write fetch a class
    /// whose elements are places answers the element as a reference,
    /// which the fetch writes through; any other value is written into
    /// nothing, with php's `Indirect modification of overloaded
    /// element` notice where it is no object.
    ///
    /// ```php
    /// class A implements ArrayAccess {
    ///     public $data = ['k' => []];
    ///     public function offsetGet($k): mixed { echo "get $k\n"; return $this->data[$k] ?? null; }
    ///     public function offsetExists($k): bool { echo "exists $k\n"; return isset($this->data[$k]); }
    ///     public function offsetSet($k, $v): void {}
    ///     public function offsetUnset($k): void {}
    /// }
    /// $a = new A;
    /// var_dump(isset($a['k']['x']), $a['nope']['x'] ?? 'dflt');
    /// $a['k']['x'] = 1;
    /// $o = new ArrayObject(['k' => []]);
    /// $o['k']['x'] = 1; $o['new'][] = 2; $o['k']['x'] .= 'y';
    /// var_dump($a->data, $o->getArrayCopy());
    /// ```
    /// ```expect
    /// exists k
    /// get k
    /// exists nope
    /// bool(false)
    /// string(4) "dflt"
    /// get k
    ///
    /// Notice: Indirect modification of overloaded element of A has no effect in %s on line 11
    /// array(1) {
    ///   ["k"]=>
    ///   array(0) {
    ///   }
    /// }
    /// array(2) {
    ///   ["k"]=>
    ///   array(1) {
    ///     ["x"]=>
    ///     string(2) "1y"
    ///   }
    ///   ["new"]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(2)
    ///   }
    /// }
    /// ```
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        req.read_dimension(object, offset, read)
    }

    /// `read_dimension` under a write fetch, `BP_VAR_W`, `BP_VAR_RW` or
    /// `BP_VAR_UNSET` (`fetch`), where php's answers a pointer into the
    /// object's own storage: the place the element stands, which the
    /// fetch lays into its `VAR` as `IS_INDIRECT`, so a nested write
    /// reaches the storage and `$o[$k] = &$v` binds it. The fetch unwraps
    /// a reference nothing else holds (`ZVAL_UNREF`), as php's does over
    /// the reference its `read_dimension` made in place. `None`, `Std`'s
    /// answer, is a class whose handler answers a value: the fetch asks
    /// [`ObjectHandlers::read_dimension`] instead.
    fn dimension_place(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        fetch: DimensionRead,
    ) -> Result<Option<Place>, Thrown> {
        let _ = (req, object, offset, fetch);
        Ok(None)
    }

    /// `write_dimension`, `zend_std_write_dimension` at `Std`:
    /// `offsetSet` on an `ArrayAccess`, and php's `Error` otherwise. A
    /// sink of the value's owner, which a table that keeps the value
    /// hands on and one that does not, or refuses the write, gives up.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.write_dimension(object, offset, value)
    }

    /// `get_property_ptr_ptr`, `zend_std_get_property_ptr_ptr` at
    /// `Std`: where a write to `name` goes, the `Err` where the class
    /// refuses any write with a throwable, php's `&EG(error_zval)` beside
    /// it (`incomplete_class_get_property_ptr_ptr`), [`PropertyPlace::Std`] for
    /// the standard place the kernel lays down itself, and
    /// [`PropertyPlace::Handlers`], php's `NULL`, for a name the class
    /// serves. The answer is a verdict with no effect, so `ASSIGN_OBJ`
    /// asks it too: php's sends every name its cache slot has not placed
    /// to `write_property`, and the cache is filled only by the standard
    /// placement, so a name a class serves reaches its `write_property`
    /// and every other name the in-place write. A class that overrides
    /// `write_property` for a name answers `Handlers` for it, whether or
    /// not the class declares it.
    ///
    /// A declared property the class serves is its handlers' on a
    /// write, as `DatePeriod`'s are, whose handlers refuse the write;
    /// and a compound write over a name `DateInterval` serves reads it
    /// through `read_property` and writes the sum back through
    /// `write_property`, with no dynamic property laid down.
    ///
    /// ```php
    /// $p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 2);
    /// try { $p->recurrences = 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($p->recurrences);
    /// $i = new DateInterval('P0Y');
    /// $i->f = 0.5;
    /// $i->f += 0.25;
    /// var_dump($i->f, $i->f++, ++$i->f);
    /// ```
    /// ```expect
    /// Cannot modify readonly property DatePeriod::$recurrences
    /// int(3)
    /// float(0.75)
    /// float(0.75)
    /// float(2.75)
    /// ```
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        Ok(req.property_place(object, name))
    }

    /// `has_property`, `zend_std_has_property` at `Std`: whether the
    /// property is there, is set, or is not empty, as `test` asks. A
    /// class with a `__isset` serves it here, and one with a `__get`
    /// beside it is asked that too under `empty`, so the answer is what
    /// a call left pending as much as what a slot holds.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        req.has_property(object, name, test)
    }

    /// `unset_property`, `zend_std_unset_property` at `Std`: the
    /// declared slot left uninitialised or the dynamic entry removed,
    /// what it held released.
    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        req.unset_property(object, name)
    }

    /// `has_dimension`, `zend_std_has_dimension` at `Std`:
    /// `offsetExists` on an `ArrayAccess`, and `offsetGet`'s truth after
    /// it under `not_empty`, `empty($o[$k])`.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        req.has_dimension(object, offset, not_empty)
    }

    /// `unset_dimension`, `zend_std_unset_dimension` at `Std`:
    /// `offsetUnset` on an `ArrayAccess`.
    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        req.unset_dimension(object, offset)
    }

    /// `get_properties`, php's `HashTable *`, which
    /// [`PhpObject::property_table`] reads: at `Std`,
    /// `zend_std_get_properties`, the object's own property table laid
    /// down as `rebuild_object_properties` lays it, the declared slots
    /// then the dynamic table, each declared row keyed by its mangled
    /// name; a lazy object initialised first and the table that of what
    /// that answers, `zend_lazy_object_get_properties`. A class that
    /// writes its fields into the table first (`DateInterval`) answers
    /// `Std`'s after; one that keeps a table in its place answers that
    /// one (`sxe_get_properties`). A view, no owner taken.
    fn get_properties(&self, req: &mut Request, object: PhpObject) -> Result<walk::Table, Thrown> {
        req.std_properties(object)
    }

    /// `get_method`, `zend_std_get_method` at `Std`: the method under
    /// the lowered name, by identity, and none where the class has no
    /// entry of that name. The access rule is the caller's, as php's
    /// `INIT_METHOD_CALL` applies it after the lookup. A class whose
    /// handler refuses the call itself, as php's answers `NULL` with a
    /// throwable left pending, answers the `Err`: `spl_recursive_it_get_method`
    /// over an iterator whose constructor never ran, and
    /// `incomplete_class_get_method` over any name.
    fn get_method(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        Ok(req.std_method(object.class(), lowered))
    }

    /// `ce->get_static_method`, asked before `zend_std_get_static_method`
    /// for a method a call names on the class: `None`, `Std`'s answer,
    /// leaves the name to the standard lookup. A class that answers a
    /// name its table has no row for answers a trampoline
    /// ([`Request::native_static_trampoline`]), as php's test
    /// extension's `_ZendTestClass` answers `test`; `written` is the name
    /// as the call wrote it.
    fn get_static_method(
        &self,
        req: &mut Request,
        class: ClassId,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        let _ = (req, class, lowered, written);
        Ok(None)
    }

    /// php's `get_method` as the slot is handed the name the call wrote
    /// beside its lowered key (`zend_object_handlers.h`: `zend_string
    /// *method_name, const zval *key`): a class whose methods are names
    /// it learns only when a call names them, and holds by case, as `FFI`
    /// holds the C functions a declaration names (`zend_ffi_get_func`),
    /// reads `written`. At `Std` and at every class keyed by the lowered
    /// name, [`ObjectHandlers::get_method`].
    fn get_method_as(
        &self,
        req: &mut Request,
        object: PhpObject,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<Option<Function>, Thrown> {
        let _ = written;
        self.get_method(req, object, lowered)
    }

    /// The object a method `get_method` did not find is looked up on
    /// and called on in its place, the `*object` php's `get_method`
    /// may replace: `spl_dual_it_get_method` hands an outer iterator's
    /// unknown method to its inner iterator, `spl_recursive_it_get_method`
    /// to the iterator of the level it stands on. A view, no owner
    /// taken; `None` at `Std`.
    ///
    /// ```php
    /// class Inner extends ArrayIterator { function hello() { return get_class($this) . " says hello"; } }
    /// $outer = new IteratorIterator(new Inner([1]));
    /// echo $outer->hello(), "\n";
    /// try { $outer->missing(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Inner says hello
    /// Call to undefined method IteratorIterator::missing()
    /// ```
    fn method_receiver(&self, req: &mut Request, object: PhpObject) -> Option<PhpObject> {
        let _ = (req, object);
        None
    }

    /// `get_constructor`, `zend_std_get_constructor` at `Std`: the
    /// class's `__construct` where the calling scope may reach it, php's
    /// `Error` where it may not, and none for a class with no
    /// constructor. A table that refuses construction in its
    /// [`Refusals`] throws php's `Error` in its template instead.
    fn get_constructor(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        if let Construction::Refused(instead) = self.refusals().construction {
            return Err(Refusals::refuse_construction(req, object, instead));
        }
        req.constructor_of(object)
    }

    /// `get_class_name`, `zend_std_get_class_name` at `Std`: `ce->name`,
    /// its bytes as they stand. What `var_dump`, `debug_zval_dump` and
    /// `print_r` print an object's class as, which `FFI\CData` answers
    /// with its C type after the class's name
    /// (`zend_ffi_cdata_get_class_name`).
    fn get_class_name(&self, req: &mut Request, object: PhpObject) -> Vec<u8> {
        req.class_name_bytes(object.class())
    }

    /// `cast_object`, `zend_std_cast_object_tostring` at `Std`: the
    /// `__toString` of the class for a string, `true` for a bool, and
    /// `None`, php's `FAILURE`, for every other type and for a class
    /// with no `__toString`. The value cast to is the caller's, as php's
    /// `retval` is.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        req.cast_object(object, to)
    }

    /// `count_elements`: what `count()` answers for the object, and
    /// `None` at `Std`, php's null slot, so `count()` asks `Countable`
    /// and throws for a class that is not.
    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        let _ = (req, object);
        Ok(None)
    }

    /// `get_debug_info`: the table `var_dump` and `print_r` show in
    /// place of the properties, php's `*is_temp` one, with one owner,
    /// the caller's. `zend_std_get_debug_info` at `Std`, which is what
    /// `__debugInfo` returns. `None` is the members themselves, which
    /// php's slot answers as the properties table it is handed
    /// (`get_properties(object)`) and a reader here walks for itself, so
    /// a typed property nothing has written still shows as one. A table
    /// built with a throwable left pending is [`DebugInfo::thrown`]; the
    /// `Err` is a table that could not be made.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        req.debug_info(object)
            .map(|table| table.map(DebugInfo::from))
    }

    /// `get_closure`: the function a call on the object serves, with
    /// its receiver and scope; `zend_std_get_closure` at `Std`,
    /// `__invoke` with the object as its receiver, and none for a class
    /// with no `__invoke`, which is not callable.
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        req.invoke_closure(object)
    }

    /// `get_gc`: the values the object's payload owns, for the cycle
    /// collector's walk, handed to `visit` as php's handler fills
    /// `*table`, and the table php's handler returns beside them. The
    /// declared slots and the dynamic table are the heap's to walk;
    /// `Std` names nothing more and hands back [`GcTable::Properties`].
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        _visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let _ = (req, object);
        GcTable::Properties
    }

    /// `do_operation`: the value an operator over the object answers,
    /// the caller's, as php's `result` is, `right` none for a unary one;
    /// `None` at `Std`, php's null slot, so the operator is
    /// `zend_binop_error`'s.
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        let _ = (req, op, left, right);
        Ok(None)
    }

    /// `compare`, `zend_std_compare_objects` at `Std`: equal for one
    /// object, uncomparable (`None`, php's `ZEND_UNCOMPARABLE`) across
    /// classes, and the declared slots then the dynamic tables in order
    /// within one class. A table that refuses comparison in its
    /// [`Refusals`] answers uncomparable for every pair,
    /// `zend_objects_not_comparable`.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        if self.refusals().comparison {
            return Ok(None);
        }
        req.compare_objects(left, right)
    }

    /// The element a `zend_object_iterator` of the class stands at
    /// `index` steps past its rewind, its key and its value, one more
    /// owner each: what an `InternalIterator` over the object answers
    /// ([`crate::api::call::internal_iterator`]), for a class php walks
    /// with an iterator of its own in C rather than with methods.
    /// `None` at `Std` and past the last element.
    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<IteratorElement>, Thrown> {
        let _ = (req, object, index);
        Ok(None)
    }

    /// The `get_current_data` of an iterator php's class hands out from
    /// its own `get_iterator` and walks in C, where that iterator can be
    /// taken by reference: `spl_array_get_iterator`'s, whose element is
    /// a place in the storage. A `foreach` by reference over an object
    /// of such a class whose `Iterator` methods are all its natives
    /// binds each element through the function this answers, and one
    /// over any other `Iterator` is php's refusal. `None` at `Std`.
    ///
    /// ```php
    /// $it = new ArrayIterator([1, 2]);
    /// foreach ($it as &$v) { $v *= 10; }
    /// unset($v);
    /// class Over extends ArrayIterator { function current(): mixed { return parent::current(); } }
    /// try { foreach (new Over([3]) as &$v) {} } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($it->getArrayCopy());
    /// ```
    /// ```expect
    /// An iterator cannot be used with foreach by reference
    /// array(2) {
    ///   [0]=>
    ///   int(10)
    ///   [1]=>
    ///   int(20)
    /// }
    /// ```
    fn current_place(&self) -> Option<CurrentPlace> {
        None
    }

    /// `ce->get_iterator` of a class that walks its objects in C, what
    /// a `foreach`, a `yield from` or a spread over an object of it
    /// walks: the iterator, or php's refusal of the walk, such as
    /// `spl_recursive_it_get_iterator`'s over an object whose
    /// constructor never ran. `None` at `Std`, php's
    /// `zend_user_it_get_iterator`, so each step serves one of the
    /// `Iterator` methods. Asked only while none of the five is a
    /// user's, since `zend_implement_iterator` puts
    /// `zend_user_it_get_iterator` in place of the class's own on a
    /// class that overrides any of them; `by_ref` is a walk by
    /// reference, which the class refuses in its words.
    ///
    /// ```php
    /// $l = new SplDoublyLinkedList; $l->push(1); $l->push(2);
    /// foreach ($l as $a) { foreach ($l as $b) echo "$a$b,"; }
    /// echo "\n";
    /// class Own extends SplDoublyLinkedList { function current(): mixed { return 'own'; } }
    /// $o = new Own; $o->push(1);
    /// foreach ($o as $v) echo $v, "\n";
    /// ```
    /// ```expect
    /// 11,12,21,22,
    /// own
    /// ```
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        let _ = (req, object, by_ref);
        Ok(None)
    }

    /// Whether the class sets `ce->get_iterator` though it is not
    /// `Traversable`: a `foreach` over its objects walks
    /// [`ObjectHandlers::get_iterator`] and never their properties, and
    /// Reflection calls the class iterable, as `FFI\CData` is
    /// (`zend_ffi_cdata_get_iterator`). `false` at `Std`, where only a
    /// `Traversable` class walks with an iterator.
    fn own_iterator(&self) -> bool {
        false
    }

    /// `sizeof` the iterator struct the class's `get_iterator` `emalloc`s,
    /// what `memory_get_usage()` counts of the iterator a `foreach` or an
    /// outer iterator holds over one of its objects (design §5.13): a
    /// declaration, as a class's `size` is, `zend_object_iterator`'s 88
    /// bytes unless the class's struct extends it (`spl_array_iterator`'s
    /// 96, `spl_dllist_it`'s 104).
    ///
    /// ```php
    /// function probe($label, $x) { foreach ($x as $v) { break; } $m = memory_get_usage(); foreach ($x as $v) { echo $label, ' ', memory_get_usage() - $m, "\n"; break; } }
    /// probe('fixed', SplFixedArray::fromArray([1, 2]));
    /// $l = new SplDoublyLinkedList; $l->push(1); probe('dllist', $l);
    /// probe('period', new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 2));
    /// $wm = new WeakMap; $k = new stdClass; $wm[$k] = 1; probe('weakmap', $wm);
    /// $d = new DOMDocument; $d->loadXML('<a><b/><c/></a>'); probe('nodes', $d->documentElement->childNodes);
    /// probe('sxe', simplexml_load_string('<a><b/><c/></a>'));
    /// $p = new PDO('sqlite::memory:'); probe('stmt', $p->query('select 1 union select 2'));
    /// probe('array', new ArrayIterator([1, 2]));
    /// $h = Dom\HTMLDocument::createFromString('<p class="a b">x</p>', LIBXML_NOERROR); probe('tokens', $h->body->firstChild->classList);
    /// ```
    /// ```expect
    /// fixed 128
    /// dllist 112
    /// period 128
    /// weakmap 96
    /// nodes 128
    /// sxe 96
    /// stmt 112
    /// array 96
    /// tokens 112
    /// ```
    fn iterator_size(&self) -> usize {
        88
    }

    /// `get_properties_for`: the table one purpose reads, each row under
    /// the key php's table holds it by, a property's name as a string
    /// key even where it reads as a number and an element a class shows
    /// among its properties as an integer one
    /// (`spl_fixedarray_object_get_properties_for`). At `Std`, php's null
    /// slot, `zend_std_get_properties_for`
    /// (`Z/Zend/zend_object_handlers.c:2608`), which a class that
    /// answers some purposes itself answers the others with: the class's
    /// `get_debug_info` first under [`PropertyPurpose::Debug`], and where
    /// it shows the members, the table `get_properties` lays down, a lazy
    /// object's read as it stands; the hooked properties' table for the
    /// readers that call hooks; and `get_properties`' table otherwise, a
    /// lazy object read as it stands under the `(array)` cast and under a
    /// serialization it was made to skip initialising for.
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        req.std_properties_for(object, purpose)
    }

    /// Whether the table's `get_properties` is not
    /// `zend_std_get_properties` or its `get_properties_for` is set: the
    /// class answers the reader of its properties itself, so its
    /// property table is not what a reader sees. What
    /// `spl_array_set_array` refuses an object as the backing of an
    /// `ArrayObject` for; `false` at `Std`, and `true` for every table
    /// that answers [`ObjectHandlers::get_properties_for`] for some
    /// purpose, and for every table php sets either slot on.
    ///
    /// ```php
    /// foreach ([new DateInterval('P1D'), new SplFixedArray(2), new stdClass] as $o) {
    ///     try { @new ArrayObject($o); echo get_class($o), " backs\n"; }
    ///     catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Overloaded object of type DateInterval is not compatible with ArrayObject
    /// Overloaded object of type SplFixedArray is not compatible with ArrayObject
    /// stdClass backs
    /// ```
    fn properties_overloaded(&self) -> bool {
        false
    }

    /// Whether the table's `write_property` is not
    /// `zend_std_write_property` and answers for a property the class
    /// declares: `ZEND_ASSIGN_OBJ` writes a declared slot in place only
    /// through the cache `zend_std_write_property` fills, so a class
    /// whose own handler guards a declared name has every assignment to
    /// it reach that handler. `false` at `Std`; `PDOStatement`'s
    /// `dbstmt_prop_write` is the witness, in `ext/pdo`.
    fn write_overloaded(&self) -> bool {
        false
    }

    /// `ce->unserialize` of an internal class that sets its own, as
    /// `GMP`'s `gmp_unserialize` is: what `object_custom` hands the
    /// payload of a `C:` record, over an object it made of the class.
    /// `None` at `Std`, php's null slot, which leaves the class to
    /// `zend_user_unserialize` where it implements `Serializable` and to
    /// php's warning where it does not
    /// ([`crate::api::call::class_unserialize`]).
    fn unserialize(&self) -> Option<ClassUnserialize> {
        None
    }
}

/// The `ce->unserialize` slot: the payload of a `C:` record read into
/// an object of the class. The `Err` is php's `FAILURE` with the
/// throwable it raised, which fails the `unserialize()` around it.
pub type ClassUnserialize = fn(&mut Request, PhpObject, &[u8]) -> Result<(), Thrown>;

/// `std_object_handlers`: every slot its default, the table a user
/// class and a native class that declares none are reached through.
#[derive(Debug, Clone, Copy, Default)]
pub struct Std;

impl ObjectHandlers for Std {}

/// The three refusals php keeps in a class's handler table, each a fact
/// of its own: a `get_constructor` that throws "Cannot directly
/// construct", a null `clone_obj`, and `compare` as
/// `zend_objects_not_comparable`. `CurlHandle` refuses construction and
/// comparison and clones; `FTP\Connection` and `GdFont` refuse
/// construction and cloning and compare. A class states its refusals
/// once, as one named constant, and [`ObjectHandlers`]' default bodies
/// serve them. The value is itself a table, `std_object_handlers` with
/// these refusals, so a class with nothing else to override names the
/// constant as its `handlers`; a table with slots of its own answers it
/// from [`ObjectHandlers::refusals`]. The constant is a named `const`
/// and not a builder chain in the class attribute, since the attribute
/// borrows its table for the engine's whole life and only a named
/// constant is promoted to that.
///
/// ```php
/// try { new CurlShareHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { clone curl_share_init(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump(curl_share_init() == curl_share_init());
/// try { new CurlHandle; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $c = curl_init();
/// var_dump(clone $c instanceof CurlHandle, $c == clone $c);
/// ```
/// ```expect
/// Cannot directly construct CurlShareHandle, use curl_share_init() instead
/// Trying to clone an uncloneable object of class CurlShareHandle
/// bool(false)
/// Cannot directly construct CurlHandle, use curl_init() instead
/// bool(true)
/// bool(false)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusals {
    construction: Construction,
    cloning: bool,
    comparison: bool,
}

/// Whether `get_constructor` refuses, and with which words after the
/// class's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Construction {
    Allowed,
    Refused(Option<&'static str>),
}

impl Refusals {
    /// Nothing refused, `std_object_handlers`.
    pub const NONE: Refusals = Refusals {
        construction: Construction::Allowed,
        cloning: false,
        comparison: false,
    };

    /// Construction refused, in php's template: "Cannot directly
    /// construct {class}, use {instead} instead", or "Cannot directly
    /// construct {class}" alone where `instead` is `None`, as
    /// `Soap\Url`'s and `Soap\Sdl`'s say. The class is the object's,
    /// which for the final classes that spell their own name in php is
    /// that name, and for a table several classes share is each one's.
    /// A refusal php throws from a `__construct` method and not from
    /// `get_constructor`, `mysqli_warning`'s and `Dom\Node`'s, is that
    /// method's and is not stated here.
    #[must_use]
    pub const fn construction(self, instead: Option<&'static str>) -> Refusals {
        Refusals {
            construction: Construction::Refused(instead),
            ..self
        }
    }

    /// Cloning refused: `clone_obj` is php's null slot, so `clone`
    /// throws "Trying to clone an uncloneable object of class {class}".
    #[must_use]
    pub const fn cloning(self) -> Refusals {
        Refusals {
            cloning: true,
            ..self
        }
    }

    /// Comparison refused: `compare` is `zend_objects_not_comparable`,
    /// uncomparable with any value.
    #[must_use]
    pub const fn comparison(self) -> Refusals {
        Refusals {
            comparison: true,
            ..self
        }
    }

    /// The `Error` a refused `get_constructor` throws, `zend_throw_error`
    /// in the template.
    fn refuse_construction(req: &mut Request, object: PhpObject, instead: Option<&str>) -> Thrown {
        let class = req.class_name(object.class());
        let message = match instead {
            Some(instead) => {
                format!("Cannot directly construct {class}, use {instead} instead")
            }
            None => format!("Cannot directly construct {class}"),
        };
        crate::api::raising::throw_error(req, "Error", &message)
    }
}

impl ObjectHandlers for Refusals {
    fn refusals(&self) -> Refusals {
        *self
    }
}

#[cfg(test)]
mod tests {
    use php_unit::decl::ClassKind;

    use super::{ExtensionBuilder, Refusals};
    use crate::api::__private::class;
    use crate::engine::Engine;
    use crate::test::Capture;

    const UNMADE: Refusals = Refusals::NONE.construction(Some("make()"));
    const UNNAMED: Refusals = Refusals::NONE.construction(None);
    const UNCLONED: Refusals = Refusals::NONE.cloning();
    const UNCOMPARED: Refusals = Refusals::NONE.comparison();

    /// Each refusal holds alone, served by the default bodies of a table
    /// that is nothing but its `Refusals`: the constructor's two
    /// templates, a constructible class that refuses only cloning and
    /// still compares, and one that refuses only comparison and still
    /// clones.
    #[test]
    fn each_refusal_is_a_fact_of_its_own() {
        let extension = ExtensionBuilder::default()
            .class_with(class(b"Unmade", ClassKind::Class), Vec::new(), &UNMADE)
            .class_with(class(b"Unnamed", ClassKind::Class), Vec::new(), &UNNAMED)
            .class_with(class(b"Uncloned", ClassKind::Class), Vec::new(), &UNCLONED)
            .class_with(
                class(b"Uncompared", ClassKind::Class),
                Vec::new(),
                &UNCOMPARED,
            )
            .build("refusals");
        let served = Engine::cli_builder()
            .extensions([extension])
            .build()
            .serve_capture(
                r#"<?php
foreach (['Unmade', 'Unnamed', 'Uncloned', 'Uncompared'] as $c) {
    try { new $c; echo "made $c\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
}
$a = new Uncloned;
try { clone $a; } catch (Error $e) { echo $e->getMessage(), "\n"; }
var_dump($a == new Uncloned);
$b = new Uncompared;
var_dump(clone $b instanceof Uncompared, $b == new Uncompared);
"#,
                Capture::default(),
            );
        assert_eq!(served.fatal, None);
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "Cannot directly construct Unmade, use make() instead\n\
             Cannot directly construct Unnamed\n\
             made Uncloned\n\
             made Uncompared\n\
             Trying to clone an uncloneable object of class Uncloned\n\
             bool(true)\n\
             bool(true)\n\
             bool(false)\n"
        );
    }
}

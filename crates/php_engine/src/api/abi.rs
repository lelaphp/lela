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
use std::sync::{Arc, Mutex};

use php_heap::{Fatal, Heap, Obj};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ConstDecl, FunctionDecl, IniDecl, MethodDecl, Name, PropertyDecl,
    Visibility,
};

use crate::api::call::{Function, Target};
use crate::api::coerce::BinaryOp;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{
    ClassId, ClosureInfo, Key, PhpArray, PhpObject, PhpRef, PhpString, Place, TypeTag, Value,
};

mod class;
mod marshal;

pub use class::{
    Cases, Class, ClassRef, ConstArray, ConstValue, Header, IntoConst, Members, PropertyValue,
    Typed,
};
pub use marshal::{
    False, FromArgument, Instance, IntoReturn, Iterable, NamedVariadic, Never, Null, NullDefault,
    OrFalse, Passed, Path, Raw, Ref, Static, This, True, Union, Untyped, Variadic,
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

/// What an ini entry's `on_modify` function says of a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniVerdict {
    Accepted,
    Rejected,
}

/// One row of `zend_module_entry.deps`, a `zend_module_dep`
/// (`Z/Zend/zend_modules.h`): the other extension's name, how this one
/// stands to it, and the `rel` and `version` a `ZEND_MOD_REQUIRED_EX`
/// row carries, which php prints after the kind and
/// `ReflectionExtension::getDependencies` reads. The engine starts its
/// extensions in the order `zend_sort_modules` leaves by the required
/// and optional rows; the check php makes that a required one is
/// loaded is `api-check`'s crate graph here.
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
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
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
        match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The value held for `key`, built by nothing: what the unit cache
    /// asks before it compiles, so a hit and a miss are told apart.
    pub(crate) fn get(&self, key: &K) -> Option<Arc<V>> {
        self.entries().get(key).map(Arc::clone)
    }

    /// `value` held for `key`, or what another thread put there while
    /// this one built it, which is kept: the one both are answered.
    pub(crate) fn insert(&self, key: K, value: V) -> Arc<V> {
        let mut entries = self.entries();
        if let Some(found) = entries.get(&key) {
            return Arc::clone(found);
        }
        self.make_room(&mut entries);
        let value = Arc::new(value);
        entries.insert(key, Arc::clone(&value));
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

    /// `value` dropped under every key it is held under.
    pub(crate) fn remove_all(&self, value: &Arc<V>) {
        self.entries().retain(|_, held| !Arc::ptr_eq(held, value));
    }

    /// Every value held, with its key, in no order.
    pub(crate) fn snapshot(&self) -> Vec<(K, Arc<V>)> {
        self.entries()
            .iter()
            .map(|(key, value)| (key.clone(), Arc::clone(value)))
            .collect()
    }

    /// The values `keep` answers false for, dropped: a value a caller
    /// still holds lives until it lets go.
    pub(crate) fn retain(&self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.entries().retain(|key, value| keep(key, value));
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
    idle: Mutex<HashMap<K, Vec<V>>>,
}

impl<K: Hash + Eq, V> Pool<K, V> {
    /// A pool keeping at most `capacity` idle entries; one given back past
    /// the bound is dropped, which closes it. The engine's own for its
    /// streams, and an extension's as a field of its engine state, as
    /// persistent PDO's is; what a request took out it holds in a
    /// [`Lent`] of its request state.
    #[must_use]
    pub fn new(capacity: usize) -> Pool<K, V> {
        Pool {
            capacity: capacity.max(1),
            idle: Mutex::new(HashMap::new()),
        }
    }

    fn idle(&self) -> std::sync::MutexGuard<'_, HashMap<K, Vec<V>>> {
        match self.idle.lock() {
            Ok(idle) => idle,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// An idle entry under `key`, lent to the caller alone: the one given
    /// back last, as php's one entry per key is the one a worker last used.
    pub fn take(&self, key: &K) -> Option<V> {
        let mut idle = self.idle();
        let entries = idle.get_mut(key)?;
        let entry = entries.pop();
        if entries.is_empty() {
            idle.remove(key);
        }
        entry
    }

    /// The entries idle under every key, which php counts as a process's
    /// inactive persistent links, `MyG(num_inactive_persistent)`.
    #[must_use]
    pub fn idle_len(&self) -> usize {
        self.idle().values().map(Vec::len).sum()
    }

    /// `entry` given back under `key` at the end of the request that
    /// held it; past the bound it is handed back to the caller, whose
    /// drop closes it outside the `Mutex`.
    pub fn give(&self, key: K, entry: V) -> Option<V> {
        let mut idle = self.idle();
        if idle.values().map(Vec::len).sum::<usize>() >= self.capacity {
            return Some(entry);
        }
        idle.entry(key).or_default().push(entry);
        None
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
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZendExtension {
    pub name: String,
    pub version: String,
    pub author: String,
    pub url: String,
    pub copyright: String,
}

pub(crate) use crate::api::__private::Entry;
/// Builds one extension's request state at request start.
pub(crate) type StateBuilder = (TypeId, fn() -> Box<dyn Any + Send>);

/// What [`ExtensionBuilder::constants_with`] was handed: the declared
/// constants rewritten with every value the bundle registered.
pub(crate) type ConstantsLink =
    Arc<dyn Fn(Vec<ConstDecl>, &[Arc<dyn Any + Send + Sync>]) -> Vec<ConstDecl> + Send + Sync>;
/// Builds one extension's engine state from the startup ini, once.
pub(crate) type EngineStateBuilder =
    Arc<dyn Fn(&StartupIni) -> Box<dyn Any + Send + Sync> + Send + Sync>;
/// The `RINIT` function.
pub(crate) type Rinit = fn(&mut Request) -> Result<(), Thrown>;
/// The interrupt hook, php's `zend_interrupt_function`.
pub(crate) type InterruptHook = fn(&mut Request) -> Result<(), Thrown>;
/// The hook `ZEND_TICKS` serves, a function php's `php_add_tick_function`
/// adds.
pub(crate) type TicksHook = fn(&mut Request) -> Result<(), Thrown>;
/// The compile hook, php's `zend_compile_file` as an extension overrides
/// it: the source a file compiles from, where the extension replaces it.
pub(crate) type CompileFileHook = fn(&mut Request, &[u8]) -> Result<Option<Vec<u8>>, Thrown>;
/// The one question the unit cache asks an extension about a file
/// (design §6.3): [`ExtensionBuilder::admission`].
pub(crate) type AdmissionHook = fn(&mut Request, &[u8]) -> Admission;
/// What the engine asks each extension as it finishes starting up:
/// [`ExtensionBuilder::post_startup`].
pub(crate) type PostStartupHook = fn(&mut Request) -> PostStartup;
/// What the unit cache tells each extension of a file it holds:
/// [`ExtensionBuilder::cached`].
pub(crate) type CachedHook = fn(&mut Request, &[u8]);
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
/// host, and the first extension in bundle order that does not answer
/// [`PostStartup::Serve`] decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostStartup {
    /// Serve as built.
    Serve,
    /// `accel_preload`: the file at the path compiled and served once, in a
    /// request of its own, before the first request, and every class and
    /// function it declared that links declared in every request after,
    /// as they stand in the unit cache. A file that ends in a fatal or
    /// an `exit` fails php's startup, and the process ends with status 1.
    Preload(Vec<u8>),
    /// Serve nothing: the process ends with `status`, as
    /// `zend_accel_error` at `ACCEL_LOG_FATAL` ends it with `exit(-2)`.
    Refuse { status: i32 },
}
/// The path resolver, php's `zend_resolve_path` as an extension overrides
/// it: [`ExtensionBuilder::resolve_path`].
pub(crate) type ResolvePathHook = fn(&mut Request, &[u8]) -> Result<Option<Vec<u8>>, Thrown>;

/// What [`ExtensionBuilder::encodings`] takes: the encoding functions
/// bound to one request.
pub(crate) type EncodingsHook = fn(&mut Request) -> Box<dyn Multibyte>;
/// The upload hook, php's `php_rfc1867_callback`:
/// [`ExtensionBuilder::upload`].
pub(crate) type UploadHook =
    fn(&mut Request, &crate::api::request::Upload<'_>) -> Result<bool, Thrown>;
/// The data treater, php's `sapi_module.treat_data`:
/// [`ExtensionBuilder::treat_data`].
pub(crate) type TreatDataHook = fn(
    &mut Request,
    crate::api::request::Input,
    &[u8],
) -> Result<Option<Vec<(Vec<u8>, Vec<u8>)>>, Thrown>;
/// The input filter, php's `sapi_module.input_filter`:
/// [`ExtensionBuilder::input_filter`].
pub(crate) type InputFilterHook = fn(
    &mut Request,
    crate::api::request::Input,
    &[u8],
    &[u8],
) -> Result<Option<crate::api::values::Value>, Thrown>;

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
/// started with (`ZEND_INI_STAGE_DEACTIVATE`); [`Request::ini_stage`]
/// says which. The value is php's `new_value`, `None` where it has none
/// (`NULL`), which an entry registered with no value is handed back as
/// the request ends or `ini_restore()` restores it, and which a rule may
/// read apart from the empty string. It raises what the entry's handler
/// raises, and answers whether the value is taken, `false` being php's
/// `FAILURE`, which leaves the entry as it was, its default at startup,
/// and makes `ini_set()` answer `false`.
pub type OnModify = fn(&mut Request, Option<&[u8]>) -> Result<bool, Thrown>;

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
/// through [`Request::info_table`].
pub(crate) type Info = fn(&mut Request);
/// A native class as an extension declares it (design §7.1): the
/// declaration, beside it each method's declaration with its entry,
/// the shape `functions` has, and the class's handler table (design
/// §5.4), none where the class takes its parent's, as php's
/// `create_object` is inherited, and [`Std`]'s at a root. The methods
/// given here are the class's; the engine places them on the
/// declaration as it links it, so the declaration's own `methods` is
/// left empty by an extension. A method's entry is served as a
/// function's is, with the receiver on the frame as `Request::this`
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
/// objects carry a `T`, as [`ExtensionBuilder::class_payload`] gives an
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

/// One extension (design §7.1): declarations paired with entries,
/// constants, ini entries, the lifecycle functions, and the two state
/// declarations. Built by [`ExtensionBuilder`], linked by the engine.
#[derive(Clone)]
pub struct Extension {
    pub(crate) name: String,
    /// `zend_module_entry.version`, what [`ExtensionBuilder::version`]
    /// declared, `PHP_VERSION` where it declared none.
    pub(crate) version: &'static str,
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    pub(crate) classes: Vec<NativeClassDecl>,
    pub(crate) constants: Vec<ConstDecl>,
    pub(crate) ini: Vec<IniDecl>,
    pub(crate) on_modify: Vec<(Vec<u8>, OnModify)>,
    pub(crate) dependencies: Vec<Dependency>,
    pub(crate) request_state: Option<StateBuilder>,
    pub(crate) engine_state: Option<EngineStateBuilder>,
    pub(crate) rinit: Option<Rinit>,
    pub(crate) rshutdown: Option<fn(&mut Request)>,
    /// What [`ExtensionBuilder::deactivate`] was handed.
    pub(crate) deactivate: Option<fn(&mut Request)>,
    pub(crate) interrupt: Option<InterruptHook>,
    pub(crate) ticks: Option<TicksHook>,
    pub(crate) compile_file: Option<CompileFileHook>,
    /// What [`ExtensionBuilder::admission`] was handed.
    pub(crate) admission: Option<AdmissionHook>,
    /// What [`ExtensionBuilder::post_startup`] was handed.
    pub(crate) post_startup: Option<PostStartupHook>,
    /// What [`ExtensionBuilder::cached`] was handed.
    pub(crate) cached: Option<CachedHook>,
    /// What [`ExtensionBuilder::fatal_logged`] was handed.
    pub(crate) fatal_logged: Option<FatalLoggedHook>,
    /// What [`ExtensionBuilder::program_ended`] was handed.
    pub(crate) program_ended: Option<fn(&mut Request)>,
    /// What [`ExtensionBuilder::shell`] was handed.
    pub(crate) shell: Option<Shell>,
    /// What [`ExtensionBuilder::input_filter`] was handed.
    pub(crate) input_filter: Option<InputFilterHook>,
    /// What [`ExtensionBuilder::treat_data`] was handed.
    pub(crate) treat_data: Option<TreatDataHook>,
    /// What [`ExtensionBuilder::upload`] was handed.
    pub(crate) upload: Option<UploadHook>,
    /// What [`ExtensionBuilder::resolve_path`] was handed.
    pub(crate) resolve_path: Option<ResolvePathHook>,
    /// What [`ExtensionBuilder::encodings`] was handed.
    pub(crate) encodings: Option<EncodingsHook>,
    /// What [`ExtensionBuilder::zend_extension`] was handed.
    pub(crate) zend: Option<ZendExtension>,
    /// What [`ExtensionBuilder::intercept`] was handed: each native of
    /// another extension this one replaces, by name, with its entry.
    pub(crate) intercepts: Vec<(Name, Entry)>,
    pub(crate) info: Option<Info>,
    /// What [`ExtensionBuilder::register`] was handed, in its order.
    pub(crate) registered: Vec<Arc<dyn Any + Send + Sync>>,
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

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extension")
            .field("name", &self.name)
            .field("functions", &self.functions.len())
            .field("classes", &self.classes.len())
            .finish_non_exhaustive()
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
    on_modify: Vec<(Vec<u8>, OnModify)>,
    dependencies: Vec<Dependency>,
    request_state: Option<StateBuilder>,
    engine_state: Option<EngineStateBuilder>,
    rinit: Option<Rinit>,
    rshutdown: Option<fn(&mut Request)>,
    deactivate: Option<fn(&mut Request)>,
    interrupt: Option<InterruptHook>,
    ticks: Option<TicksHook>,
    compile_file: Option<CompileFileHook>,
    admission: Option<AdmissionHook>,
    post_startup: Option<PostStartupHook>,
    cached: Option<CachedHook>,
    fatal_logged: Option<FatalLoggedHook>,
    program_ended: Option<fn(&mut Request)>,
    shell: Option<Shell>,
    input_filter: Option<InputFilterHook>,
    treat_data: Option<TreatDataHook>,
    upload: Option<UploadHook>,
    resolve_path: Option<ResolvePathHook>,
    encodings: Option<EncodingsHook>,
    zend: Option<ZendExtension>,
    intercepts: Vec<(Name, Entry)>,
    info: Option<Info>,
    registered: Vec<Arc<dyn Any + Send + Sync>>,
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
    /// and its receiver as `Request::this`.
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

    /// One native class whose objects carry a payload of `T` (design
    /// §5.4): the declaration and its methods as
    /// [`ExtensionBuilder::class_with`] takes them, the [`ObjectHandlers`]
    /// that release and walk what the payload holds, and `T` itself as
    /// the class's `create_object`. `new` lays the payload down as
    /// `T::default()` before the constructor runs, so a method reaches
    /// it through [`PhpObject::native_mut`] with no allocation of its
    /// own; a subclass takes it as it takes the table, since php
    /// inherits `create_object`.
    #[must_use]
    pub fn class_payload<T: Any + Send + Default>(
        mut self,
        class: ClassDecl,
        methods: Vec<(MethodDecl, Entry)>,
        handlers: &'static dyn ObjectHandlers,
    ) -> ExtensionBuilder {
        self.classes.push((
            class,
            methods,
            Some(handlers),
            Some(new_with_payload::<T> as NativeNew),
        ));
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
        let mut class = C::header();
        let (constants, properties, methods) = C::members();
        class.constants.extend(constants);
        class.properties.extend(properties);
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
        self.on_modify.push((name.to_vec(), handler));
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
    /// through [`Request::info_table`]; an extension without one gets
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

    /// Declare engine state: built once in `Engine::new` from the startup
    /// ini, immutable afterwards, shared by every request (ADR-0005).
    #[must_use]
    pub fn engine_state<E: Send + Sync + 'static>(
        mut self,
        build: fn(&StartupIni) -> E,
    ) -> ExtensionBuilder {
        self.engine_state = Some(Arc::new(move |ini: &StartupIni| {
            Box::new(build(ini)) as Box<dyn Any + Send + Sync>
        }));
        self
    }

    /// The request-start function, `RINIT`.
    #[must_use]
    pub fn rinit(mut self, hook: fn(&mut Request) -> Result<(), Thrown>) -> ExtensionBuilder {
        self.rinit = Some(hook);
        self
    }

    /// The request-end function, `RSHUTDOWN`, served with every extension's
    /// request state still live.
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

    /// The interrupt hook, php's `zend_interrupt_function` as pcntl's
    /// `MINIT` sets it: served by the loop's interrupt check when the
    /// request's [`Interrupt`](crate::api::request::Interrupt) was raised
    /// by something other than the execution timer (design §5.8). What
    /// it throws is left pending where the loop stands, as
    /// `zend_interrupt_helper`
    /// leaves `EG(exception)`.
    #[must_use]
    pub fn interrupt(mut self, hook: InterruptHook) -> ExtensionBuilder {
        self.interrupt = Some(hook);
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
        self.ticks = Some(hook);
        self
    }

    /// `zend_compile_file` overridden, as phar's `MINIT` overrides it
    /// with `phar_compile_file`: before a file is compiled, by an
    /// include, `spl_autoload` or as the primary file, `hook` is handed
    /// its path and may answer the source to compile under that path in
    /// place of the file's own, which the engine compiles as the
    /// request's own unit and publishes nowhere; `None` leaves the file
    /// to the unit cache. Hooks are asked in bundle order and the first
    /// source answered is compiled.
    #[must_use]
    pub fn compile_file(mut self, hook: CompileFileHook) -> ExtensionBuilder {
        self.compile_file = Some(hook);
        self
    }

    /// The one question the unit cache asks about a file (design §6.3,
    /// revision 45), what opcache's `persistent_compile_file` decides:
    /// `hook` is handed the file's full path, resolved, and answers
    /// whether the file is kept out of the cache and, where it is held,
    /// how a hit is found and validated ([`Admission`]). The first
    /// extension in bundle order with a hook answers; with none, every
    /// file is held and validated on every hit.
    #[must_use]
    pub fn admission(mut self, hook: fn(&mut Request, &[u8]) -> Admission) -> ExtensionBuilder {
        self.admission = Some(hook);
        self
    }

    /// Asked once as the engine finishes starting up, on a request of
    /// the first host's that serves no program (design §6.3): php's
    /// `zend_post_startup_cb`, where opcache refuses a configuration it
    /// cannot start under and names the file `opcache.preload` serves.
    /// What the hook writes to the error stream is written before
    /// anything the engine serves.
    #[must_use]
    pub fn post_startup(mut self, hook: fn(&mut Request) -> PostStartup) -> ExtensionBuilder {
        self.post_startup = Some(hook);
        self
    }

    /// Told of each file the unit cache compiles and holds, by its full
    /// path, on the request that compiled it (design §6.3): where php's
    /// `cache_script_in_shared_memory` has persisted a file, and opcache
    /// logs `Cached script` at `ACCEL_LOG_INFO`. Every extension with a
    /// hook is told, in bundle order.
    #[must_use]
    pub fn cached(mut self, hook: fn(&mut Request, &[u8])) -> ExtensionBuilder {
        self.cached = Some(hook);
        self
    }

    /// The host's log for a fatal, `sapi_module.log_message` as phpdbg's
    /// `php_sapi_phpdbg_log_message` serves it (`Z/sapi/phpdbg/phpdbg.c:731-770`):
    /// asked once the host's log has taken a diagnostic of a fatal level,
    /// with the request standing where the fatal was raised and before
    /// `php_error_cb` displays it, writes the status and bails out, so
    /// what the hook serves runs on the frames the fatal left standing.
    /// Hooks are asked in bundle order until one answers
    /// [`AfterLog::BailedOut`]. An extension a host declares for itself
    /// is the one that hands it (ADR-0008, #1348).
    #[must_use]
    pub fn fatal_logged(mut self, hook: fn(&mut Request) -> AfterLog) -> ExtensionBuilder {
        self.fatal_logged = Some(hook);
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
        self.program_ended = Some(hook);
        self
    }

    /// The CLI's interactive shell ([`Shell`]): what `readline_cli.c`'s
    /// `MINIT` writes into the table of a host that carries one. The
    /// last extension in bundle order with a shell is the one served,
    /// as the last `MINIT` to write the table wins.
    #[must_use]
    pub fn shell(mut self, shell: Shell) -> ExtensionBuilder {
        self.shell = Some(shell);
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
        self.input_filter = Some(hook);
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
        self.treat_data = Some(hook);
        self
    }

    /// `zend_resolve_path` overridden, as phar's `MINIT` overrides it with
    /// `phar_resolve_path`: a path an include or an open along the
    /// `include_path` names is handed to `hook` first, which may answer
    /// where it stands, a `phar://` URL inside the archive being served;
    /// `None` leaves it to the engine's own resolution. Hooks are asked in
    /// bundle order and the first answer is taken.
    #[must_use]
    pub fn resolve_path(mut self, hook: ResolvePathHook) -> ExtensionBuilder {
        self.resolve_path = Some(hook);
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
        self.encodings = Some(bind);
        self
    }

    /// The upload hook, php's `php_rfc1867_callback` as `ext/session`'s
    /// `MINIT` sets it for `session.upload_progress`: each step of a
    /// multipart body's reading is handed to `hook` as it is taken
    /// ([`crate::api::request::Upload`]), and `false` answers php's
    /// `FAILURE`: the body's reading stops at its start, a field is not
    /// registered, a file is passed over at its start and cancelled with
    /// `UPLOAD_ERR_EXTENSION` after it. Every extension's hook is asked,
    /// in bundle order.
    #[must_use]
    pub fn upload(mut self, hook: UploadHook) -> ExtensionBuilder {
        self.upload = Some(hook);
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
    /// extensions that intercept one native stack in bundle order, the
    /// later over the earlier, as opcache's handler is saved over
    /// phar's. A name no extension declares, or one `disable_functions`
    /// took out, is intercepted by nothing.
    #[must_use]
    pub fn intercept(
        mut self,
        name: &[u8],
        entry: fn(&mut Request) -> Result<Value, Thrown>,
    ) -> ExtensionBuilder {
        self.intercepts.push((Name(name.into()), entry));
        self
    }

    /// A value this extension hands at startup to the extension
    /// that reads every value of its type, php's `php_pdo_register_driver`
    /// into `pdo_driver_hash` from `pdo_sqlite`'s `MINIT`: immutable once the
    /// engine is built, and answered by [`Request::registered`] in bundle
    /// order, which is `MINIT` order (ADR-0005).
    #[must_use]
    pub fn register<T: Send + Sync + 'static>(mut self, value: T) -> ExtensionBuilder {
        self.registered.push(Arc::new(value));
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
                .iter()
                .filter_map(|value| value.downcast_ref::<T>())
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
            name: name.to_owned(),
            version: self.version.unwrap_or(crate::api::request::PHP_VERSION),
            functions: self.functions,
            classes: self.classes,
            constants: self.constants,
            ini: self.ini,
            on_modify: self.on_modify,
            dependencies: self.dependencies,
            request_state: self.request_state,
            engine_state: self.engine_state,
            rinit: self.rinit,
            rshutdown: self.rshutdown,
            deactivate: self.deactivate,
            interrupt: self.interrupt,
            ticks: self.ticks,
            compile_file: self.compile_file,
            admission: self.admission,
            post_startup: self.post_startup,
            cached: self.cached,
            fatal_logged: self.fatal_logged,
            program_ended: self.program_ended,
            shell: self.shell,
            input_filter: self.input_filter,
            treat_data: self.treat_data,
            upload: self.upload,
            resolve_path: self.resolve_path,
            encodings: self.encodings,
            zend: self.zend,
            intercepts: self.intercepts,
            info: self.info,
            registered: self.registered,
            constants_link: self.constants_link,
            class_constants: self.class_constants,
            trampolines: self.trampolines,
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

    /// `zend_fcall_info_cache` as `zend_is_callable_ex` filled it: what
    /// the callable resolved to when it was made, which is what serving
    /// it enters. php holds the same three fields for the same span,
    /// `zend_fcc_dup` past the frame the callable was read off.
    pub(crate) const fn target(&self) -> Target {
        self.target
    }

    /// The value the callable came from, a view and no owner taken. A
    /// list that holds the callable past the call frame it was read off
    /// takes its owners with [`Callable::addref`], not of this alone.
    pub(crate) const fn value(&self) -> Value {
        self.value
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

    /// `zend_fcc_addref` (`Z/Zend/zend_API.h:773`): the owners a list
    /// that keeps the callable past the frame it was read off takes, its
    /// value's and, `GC_ADDREF(fcc->object)`, the receiver's it resolved
    /// to. A `Class::method` string taken with `$this` in scope names an
    /// object the string does not own, so the list owns it itself. A
    /// closure's owner is its value's, since `fcc->closure` is the value.
    /// Given up by [`Callable::release`].
    pub(crate) fn addref(&self, req: &Request) {
        req.addref(&self.value);
        if let Target::Method {
            this: Some(this), ..
        } = self.target
        {
            req.addref(&this.as_value());
        }
    }

    /// `zend_fcc_dtor` (`Z/Zend/zend_API.h:798`): the owners
    /// [`Callable::addref`] took given up, the receiver's first,
    /// `OBJ_RELEASE(fcc->object)`, and then the value's.
    pub(crate) fn release(self, req: &mut Request) {
        if let Target::Method {
            this: Some(this), ..
        } = self.target
        {
            req.release(this.as_value());
        }
        req.release(self.value);
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

/// What a class's `get_properties_for` answers for one purpose: no slot,
/// a table of its own, or php's `NULL` table, which no reader may stand
/// in for with an empty one, since `get_object_vars()`, the `(array)`
/// cast and `get_mangled_object_vars()` answer the process's empty
/// array for it (`ZVAL_EMPTY_ARRAY`, `Z/Zend/zend_builtin_functions.c:849`,
/// `Z/Zend/zend_execute.h:280`) where a table, however empty, is copied
/// into an array of the request's.
#[derive(Debug)]
pub enum PropertiesFor {
    /// No slot of the class's own, `Std`'s answer: the reader reads
    /// `get_properties`, as `zend_std_get_properties_for` does.
    Standard,
    /// A table the class built for this reader (`*is_temp`), each row
    /// under the key php's table holds it by.
    Rows(Vec<(Key, Value)>),
    /// The slot's `NULL`: no table, which a dumper, an encoder and a
    /// serializer read as no properties
    /// (`attributes_sensitive_parameter_value_get_properties_for`).
    Null,
}

/// What the `get_debug_info` slot answers: php's `HashTable *` with its
/// `*is_temp` owner, the caller's, and the throwable a handler raised
/// while it built the table and left in `EG(exception)`, which
/// `dom_get_debug_info_helper` does for a property whose reader fails
/// (`Z/ext/dom/php_dom.c:526`). A dumper renders the table and then
/// hands the throwable on, so the dump is printed and the throwable is
/// thrown once the dumper returns, as `php_var_dump` leaves it
/// (`Z/ext/standard/var.c:184`). A second throwable the handler raises
/// is chained onto the first with [`Thrown::chain`].
#[derive(Debug)]
pub struct DebugInfo {
    pub table: PhpArray,
    pub thrown: Option<Thrown>,
}

impl From<PhpArray> for DebugInfo {
    fn from(table: PhpArray) -> DebugInfo {
        DebugInfo {
            table,
            thrown: None,
        }
    }
}

/// The `clone_obj` slot: a fresh object holding what this one holds,
/// one owner, the caller's.
pub type CloneObj = fn(&mut Request, PhpObject) -> Result<PhpObject, Thrown>;
/// The `clone_obj_with` slot: `clone($o, [...])`, the clone with each
/// named property written through `write_property` from `scope`.
pub type CloneObjWith =
    fn(&mut Request, PhpObject, Option<ClassId>, PhpArray) -> Result<PhpObject, Thrown>;
/// The `current_place` slot: the element the object's own iterator
/// stands at made a reference in place, the cell with one more owner,
/// the caller's; `None` past the last element.
pub type CurrentPlace = fn(&mut Request, PhpObject) -> Result<Option<PhpRef>, Thrown>;

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
    /// `funcs->rewind`.
    fn rewind(&mut self, req: &mut Request, object: PhpObject) -> Result<(), Thrown>;
    /// `funcs->valid`, `SUCCESS` as `true`.
    fn valid(&mut self, req: &mut Request, object: PhpObject) -> Result<bool, Thrown>;
    /// `funcs->get_current_data`, one owner, the caller's.
    fn current(&mut self, req: &mut Request, object: PhpObject) -> Result<Value, Thrown>;
    /// `funcs->get_current_key`, one owner, the caller's.
    fn key(&mut self, req: &mut Request, object: PhpObject) -> Result<Value, Thrown>;
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
pub trait ObjectHandlers: Send + Sync {
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
    /// heap given back before the payload is dropped, with the members
    /// already freed and the object's bucket already invalid. `Std`
    /// releases nothing, a class with no payload naming nothing.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let _ = (req, object);
    }

    /// `dtor_obj`, `zend_objects_destroy_object`: the class's
    /// `__destruct` served on the object, which the store holds at one
    /// owner meanwhile.
    fn dtor_obj(&self, req: &mut Request, object: PhpObject) {
        req.destruct(object);
    }

    /// `clone_obj`, `zend_objects_clone_obj` at `Std`: the members
    /// copied with one more owner each and `__clone` served on the copy.
    /// `None` is php's null slot, an uncloneable class.
    fn clone_obj(&self) -> Option<CloneObj> {
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
    /// owner of the value read, a reference followed. `read` is php's
    /// `type` argument, `BP_VAR_R` or the `BP_VAR_IS` an `isset`, an
    /// `empty` or a `??` reads through.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        req.read_property(object, name, read)
    }

    /// `write_property`, `zend_std_write_property` at `Std`: `value` is
    /// the object's from here and `name` is given up once the object
    /// has taken its own.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
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
    ) -> Result<Value, Thrown> {
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
    /// `offsetSet` on an `ArrayAccess`, and php's `Error` otherwise.
    /// `value` is given up either way.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
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

    /// `get_properties`, `zend_std_get_properties` at `Std`: the
    /// declared slots that hold a value then the dynamic table, in
    /// `rebuild_object_properties`' order; views, no owner taken. A
    /// declared slot is keyed by its name unmangled, and a dynamic row by
    /// the table's key, an integer one as the integer it is. A lazy
    /// object is initialised first and the rows are those of what that
    /// answers, `zend_lazy_object_get_properties`.
    fn get_properties(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Vec<(Key, Value)>, Thrown> {
        let object = req.lazy_init(object)?;
        req.object_entries(object.heap())
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
    /// constructor.
    fn get_constructor(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
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
    /// with no `__toString`.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Value>, Thrown> {
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
    /// collector's walk. The declared slots and the dynamic table are
    /// the heap's to walk; `Std` names nothing more.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        let _ = (req, object, visit);
    }

    /// `do_operation`: the value an operator over the object answers,
    /// `right` none for a unary one; `None` at `Std`, php's null slot,
    /// so the operator is `zend_binop_error`'s.
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Value>, Thrown> {
        let _ = (req, op, left, right);
        Ok(None)
    }

    /// `compare`, `zend_std_compare_objects` at `Std`: equal for one
    /// object, uncomparable (`None`, php's `ZEND_UNCOMPARABLE`) across
    /// classes, and the declared slots then the dynamic tables in order
    /// within one class.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
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
    ) -> Result<Option<(Value, Value)>, Thrown> {
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

    /// `get_properties_for`: the rows one purpose reads, each under the
    /// key php's table holds it by, a property's name as a string key
    /// even where it reads as a number and an element a class shows
    /// among its properties as an integer one
    /// (`spl_fixedarray_object_get_properties_for`);
    /// [`PropertiesFor::Standard`] at `Std`, php's null slot, so every
    /// purpose reads `get_properties`.
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        let _ = (req, object, purpose);
        Ok(PropertiesFor::Standard)
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

    /// Whether the table's `get_properties` answers rows of its own
    /// making, not the object's property table as
    /// `zend_std_get_properties` holds it: `sxe_get_properties`, which
    /// builds a table of the element's children on each call. The
    /// internal pointer the pointer natives move over an object
    /// ([`PhpObject::pointer_move`](crate::api::values::PhpObject::pointer_move))
    /// is the object's table's, so over such a class they read a table
    /// fresh on each call, which is php's answer for `SimpleXMLElement`, whose
    /// table is rebuilt between calls. `false` at `Std`, at every class
    /// that sets `get_properties_for` alone, and at one whose
    /// `get_properties` writes into the object's own table and answers
    /// it (`date_object_get_properties_interval`), whose pointer natives
    /// read the object's own table as php's do.
    ///
    /// ```php
    /// $x = simplexml_load_string('<a><b>1</b><c>2</c></a>');
    /// var_dump(@next($x), @key($x));
    /// $f = new SplFixedArray(2); $f[0] = 5;
    /// var_dump(@current($f), @current(new ArrayObject([7])));
    /// ```
    /// ```expect
    /// string(1) "2"
    /// string(1) "b"
    /// bool(false)
    /// bool(false)
    /// ```
    fn answers_properties(&self) -> bool {
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

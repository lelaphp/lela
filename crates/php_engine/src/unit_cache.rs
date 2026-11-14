//! The unit cache (design §6.3): what a file compiled to, held for every
//! request that asks for the same file, over the engine's one [`Cache`]
//! (ADR-0005), keyed by path and validated as the one question it asks
//! an extension answers (revision 45, [`crate::api::abi::Admission`]),
//! with the counters opcache keeps beside its shared hash. php's
//! `zend_persistent_script` is [`Held`], its `dynamic_members` the hits,
//! the last use and the next revalidation on it; the restart counters
//! are [`UnitCache`]'s, and `ZCSG(hash)`, what is counted of the memory
//! under it, `ZCSG(restart_pending)` and the `ZCSG(hits)`,
//! `ZCSG(misses)` and `ZCSG(blacklist_misses)` that
//! `zend_reset_cache_vars` zeroes are each epoch's [`Files`], which a
//! restart replaces with the epoch's arena (design §5.14). What reads
//! them is the request's face, [`crate::api::request::UnitCache`], which
//! `opcache_*` answer from, over the files of the request's epoch.

mod compact;
mod disk;
mod wire;

use std::borrow::Cow;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use php_compile::CompileError;
use php_unit::{PublishedUnit, Unit};

use crate::api::abi::{Cache, Validation};
use crate::api::request::{CacheBudget, CacheStatus, CachedFile, Preloaded, Restart, TierStatus};

use disk::build_identity;
pub(crate) use disk::{BUILD, Disk};
pub(crate) use wire::decode;

/// What the unit cache answers for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read.
pub(crate) type Cached = Option<Result<PublishedUnit, CompileRefused>>;

/// A compile the compiler refused, as a request raises it: php's
/// refusal, and what the compile had declared before it
/// ([`php_compile::Declared`]) published for the request, which php's
/// `zend_bailout` out of the compile leaves standing in its tables.
#[derive(Debug, Clone)]
pub(crate) struct CompileRefused {
    /// The refusal, what it declared taken into `declared`.
    pub(crate) error: CompileError,
    pub(crate) declared: Option<Box<php_compile::Declared<PublishedUnit>>>,
}

impl CompileRefused {
    /// `error`, what it declared published by `publish`.
    pub(crate) fn published(
        mut error: CompileError,
        publish: impl FnOnce(Unit) -> PublishedUnit,
    ) -> CompileRefused {
        let declared = error.declared.take().map(|declared| {
            let php_compile::Declared {
                unit,
                raised_before,
            } = *declared;
            Box::new(php_compile::Declared {
                unit: publish(unit),
                raised_before,
            })
        });
        CompileRefused { error, declared }
    }
}

/// The classes a held unit's compiler bound early, each published into
/// the engine's arena as the unit was held (design §5.14, §6.3), by its
/// declaration's index and by the identity the arena minted for its row
/// ([`crate::arena::EpochClassId`]): none for a class the unit leaves to
/// an opline and for one the arena could not take
/// ([`crate::api::request::Request::publish_unit`]).
pub(crate) type EarlyBound = Arc<[Option<crate::arena::EpochClassId>]>;

/// What the unit cache files a held file under, one space of names as
/// php's `ZCSG(hash)` is (design §6.3, revision 45): the file's full
/// path, and beside it the path as written, `accel_make_persistent_key`.
/// The file's stamp is on the [`Held`] file and not in the name, so one
/// path holds one file.
///
/// A name is borrowed while it is looked up and owned once it is filed,
/// as `accel_make_persistent_key` hands an absolute path back as it was
/// written and copies nothing until `zend_accel_add_key` interns it: the
/// cache's names are [`FiledKey`]s, made only from a `UnitKey`
/// ([`UnitKey::filed`]), and a lookup by a borrowed one hashes its bytes
/// as the filed one's are hashed (`Borrow<[u8]>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UnitKey<'a>(Cow<'a, [u8]>);

impl std::borrow::Borrow<[u8]> for UnitKey<'_> {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

/// A [`UnitKey`] as the cache files it, its bytes owned: the key of
/// [`Files`]' names, which holds no borrow. It hashes and compares as its
/// bytes, as a `UnitKey` does, so a lookup by a borrowed name's bytes
/// finds it (`Borrow<[u8]>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FiledKey(Box<[u8]>);

impl std::borrow::Borrow<[u8]> for FiledKey {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl FiledKey {
    /// The name's bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}

impl<'a> UnitKey<'a> {
    /// The name of the file at the full path `path`, which is also an
    /// absolute path as written, borrowed from `path`.
    pub(crate) fn full(path: &'a [u8]) -> UnitKey<'a> {
        UnitKey(Cow::Borrowed(path))
    }

    /// The name's bytes.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// The name as the cache files it, its bytes copied where they were
    /// borrowed.
    pub(crate) fn filed(self) -> FiledKey {
        FiledKey(self.0.into_owned().into_boxed_slice())
    }
}

impl UnitKey<'static> {
    /// `accel_make_persistent_key` for a relative path as written: the
    /// path with what resolves it beside it, the working directory `cwd`,
    /// the `include_path` and the directory of the file `including` it,
    /// each apart by a byte no path holds.
    pub(crate) fn relative(
        written: &[u8],
        cwd: &[u8],
        include_path: &[u8],
        including: Option<&[u8]>,
    ) -> UnitKey<'static> {
        let mut key = [written, cwd, include_path].join(&0);
        if let Some(file) = including {
            let dir = file
                .iter()
                .rposition(|b| *b == b'/')
                .map_or(&[][..], |at| &file[..at]);
            key.push(0);
            key.extend_from_slice(dir);
        }
        UnitKey(Cow::Owned(key))
    }
}

/// The full path of the file `path` names, as the cache files it: an
/// absolute path as it is, and a relative one made whole against `cwd`
/// where it resolves.
pub(crate) fn full_path(path: &[u8], cwd: &Path) -> Vec<u8> {
    if path.starts_with(b"/") {
        return path.to_vec();
    }
    std::fs::canonicalize(cwd.join(String::from_utf8_lossy(path).as_ref())).map_or_else(
        |_| path.to_vec(),
        |full| full.into_os_string().into_encoded_bytes(),
    )
}

/// Who stats a file the cache holds, `zend_get_file_handle_timestamp`'s
/// caller: a request, which reaches a stream wrapper's `url_stat`
/// (`crate::api::request::Request`'s), or no request, which stats a
/// plain file alone.
pub(crate) trait Stat {
    /// The file at the full path `path` as it stands now; none where
    /// nothing is there or nothing keeps a time for it.
    fn stamp(&mut self, path: &[u8]) -> Option<Stamp> {
        Stamp::of(path)
    }
}

impl Stat for () {}

/// What the system reported of a file as it was compiled: when it was
/// last written, the `zend_stat_t` field php's `validate_timestamps`
/// reads, and for a hit that php without opcache would have read from
/// disk its size, its inode and when the inode last changed. Those last
/// two tell apart a file `rename(2)` put over another with the same
/// `mtime` and the same length, which `touch()` makes of a write-then-move.
///
/// ```php
/// $f = __DIR__ . '/stamp.inc';
/// foreach ([1, 2] as $v) { file_put_contents("$f.t", "<?php return $v;"); touch("$f.t", 1000000); rename("$f.t", $f); var_dump(include $f); }
/// unlink($f);
/// ```
/// ```expect
/// int(1)
/// int(2)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
pub(crate) struct Stamp {
    modified: Duration,
    size: u64,
    inode: u64,
    changed: (i64, i64),
}

impl Stamp {
    /// The plain file at `path` as the system reports it now; none where
    /// nothing is there or the system keeps no time for it.
    pub(crate) fn of(path: &[u8]) -> Option<Stamp> {
        let described = std::fs::metadata(String::from_utf8_lossy(path).as_ref()).ok()?;
        let modified = described.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Stamp {
            modified,
            size: described.len(),
            inode: described.ino(),
            changed: (described.ctime(), described.ctime_nsec()),
        })
    }

    /// What a wrapper's `url_stat` reported, laid out as `php_fstat`
    /// lays a `zend_stat_t` out: its `st_mtime` in whole seconds and its
    /// `st_size`.
    pub(crate) fn stat(sb: [i64; 13]) -> Stamp {
        Stamp {
            modified: Duration::from_secs(u64::try_from(sb[9]).unwrap_or(0)),
            size: u64::try_from(sb[7]).unwrap_or(0),
            inode: u64::try_from(sb[1]).unwrap_or(0),
            changed: (sb[10], 0),
        }
    }

    /// `st_mtime`, the whole seconds php compares and never the size.
    pub(crate) fn seconds(self) -> i64 {
        i64::try_from(self.modified.as_secs()).unwrap_or(i64::MAX)
    }
}

/// One file the cache holds, php's `zend_persistent_script`: the unit
/// it compiled to, never a refusal, which php persists none of, the path
/// it was compiled from and the stamp it was compiled at, and its
/// `dynamic_members`, the hits since it was cached, when it was last
/// served and when it is next validated. The unit is
/// never changed; the members are atomics a racing request may write
/// over, which costs at most one more stat.
pub(crate) struct Held {
    pub(crate) unit: PublishedUnit,
    /// Its early-bound classes, published as it was held, which a
    /// request loading the unit declares by their numbers.
    pub(crate) early: EarlyBound,
    /// The full path, php's `script.filename`.
    pub(crate) path: Vec<u8>,
    /// The full path as the engine's interned string, a value no request
    /// owns, hashed before the file was held ([`UnitCache::name`]), which
    /// `EG(included_files)` is handed for a hit as `persistent_compile_file`
    /// hands it `script.filename`: no string made and no hash taken. A
    /// null where the engine's table had no room for it.
    name: php_heap::FrozenZval,
    /// The file as it was compiled; none for one held under
    /// [`Validation::Never`], php's zero timestamp, which a window never
    /// validates and nothing else finds fresh.
    stamp: Option<Stamp>,
    /// The bytes it was compiled from.
    size: u64,
    /// The order its full path entered the cache's names in, which its
    /// place in its bucket's chain is ([`UnitCache::status`]): a recompile
    /// in place of a discarded copy keeps the copy's.
    order: u64,
    hits: AtomicU64,
    last_used: AtomicI64,
    /// `dynamic_members.revalidate`: the request time up to which a hit
    /// under a window is served without a stat.
    revalidate: AtomicI64,
    /// `corrupted`: discarded, its weight counted as wasted once. It
    /// stays filed and listed, a miss to every lookup, until a compile
    /// of its path replaces it or the epoch goes.
    discarded: AtomicBool,
}

/// What a file read from disk came to, for [`UnitCache::hold`]: the
/// unit, its early-bound classes published, the bytes it was read from
/// and its stamp as observed.
struct Read {
    unit: PublishedUnit,
    early: EarlyBound,
    size: u64,
    observed: Option<Stamp>,
}

/// What a hit on a held file is held to before it is served.
enum Due {
    /// Nothing: it is served as held.
    Nothing,
    /// The file's `mtime` in whole seconds, `was`, and `next` recorded
    /// as the file's revalidation time where it matches.
    Seconds { was: i64, next: i64 },
    /// The file to the nanosecond and the byte, which a file held with
    /// no stamp never matches.
    Exact(Option<Stamp>),
}

impl Held {
    /// One of the preload's saved scripts, `preload_script_in_shared_memory`'s:
    /// held with no stamp, which nothing validates, and never served
    /// before, `last_used` zero.
    /// Its full path is `path`, interned as `name` ([`UnitCache::name`]).
    pub(crate) fn preloaded(
        unit: PublishedUnit,
        (name, path): (php_heap::FrozenZval, Vec<u8>),
        size: u64,
        order: u64,
    ) -> Held {
        Held {
            unit,
            early: Arc::new([]),
            path,
            name,
            stamp: None,
            size,
            order,
            hits: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            revalidate: AtomicI64::new(0),
            discarded: AtomicBool::new(false),
        }
    }

    /// The engine's interned string for its full path, `script.filename`,
    /// its hash cached; none where the engine's table had no room for it.
    pub(crate) fn name(&self) -> Option<php_heap::Str> {
        self.name.zval().as_str()
    }

    /// What a hit under `validation` at the request time `now` is due.
    fn due(&self, validation: Validation, now: i64) -> Due {
        match (validation, self.stamp) {
            (Validation::Never, _) | (Validation::Window { .. }, None) => Due::Nothing,
            (Validation::Window { seconds }, Some(stamp)) => {
                if seconds > 0 && self.revalidate.load(Ordering::Relaxed) >= now {
                    Due::Nothing
                } else {
                    Due::Seconds {
                        was: stamp.seconds(),
                        next: now.saturating_add(i64::from(seconds)),
                    }
                }
            }
            (Validation::EveryHit, stamp) => Due::Exact(stamp),
        }
    }

    /// Whether it was discarded, which every lookup takes for a miss.
    fn discarded(&self) -> bool {
        self.discarded.load(Ordering::Relaxed)
    }

    /// Whether a hit under `validation` at `now` is served without a
    /// stat.
    pub(crate) fn trusted(&self, validation: Validation, now: i64) -> bool {
        matches!(self.due(validation, now), Due::Nothing)
    }

    /// `validate_timestamp_and_record`: whether a hit under `validation`
    /// at `now` is served, the file as it stands stat'ed only where a
    /// hit is due, and the next revalidation time recorded where a
    /// window's stat finds the file unchanged.
    pub(crate) fn validated(&self, validation: Validation, now: i64, stat: &mut dyn Stat) -> bool {
        match self.due(validation, now) {
            Due::Nothing => true,
            Due::Seconds { was, next } => {
                let fresh = stat.stamp(&self.path).map(Stamp::seconds) == Some(was);
                if fresh {
                    self.revalidate.store(next, Ordering::Relaxed);
                }
                fresh
            }
            Due::Exact(held) => held.is_some() && stat.stamp(&self.path) == held,
        }
    }
}

/// `zend_accel_restart_reason`: why a restart was scheduled, which of
/// `ZCSG(oom_restarts)`, `ZCSG(hash_restarts)` and
/// `ZCSG(manual_restarts)` it counts under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, php_macros::EngineState)]
enum Reason {
    /// `ACCEL_RESTART_OOM`: the memory exhausted.
    Oom,
    /// `ACCEL_RESTART_HASH`: the names a file is filed under full.
    Hash,
    /// `ACCEL_RESTART_USER`: `opcache_reset`.
    User,
}

/// The unit cache's counters, which outlive a restart; the files it
/// holds, and what is counted of them until the next restart, are the
/// arena's of each epoch ([`Files`]).
pub(crate) struct UnitCache {
    taken: AtomicU64,
    /// `ZCSG(start_time)`: when the engine was built.
    start_time: i64,
    last_restart_time: AtomicI64,
    manual_restarts: AtomicU64,
    oom_restarts: AtomicU64,
    hash_restarts: AtomicU64,
    /// The bounds `opcache.memory_consumption`,
    /// `opcache.max_wasted_percentage` and `opcache.max_accelerated_files`
    /// set, once the extension has told them; none bounds nothing.
    budget: OnceLock<CacheBudget>,
    /// What `opcache.preload` preloaded, `ZCSG(preload_script)` and its
    /// saved scripts, published once before the first request.
    preloaded: OnceLock<Preloaded>,
    /// The build's identity, php's `zend_system_id`, which every unit
    /// the second level keeps carries and is read back by ([`Disk`]).
    pub(crate) build: [u8; BUILD],
    /// The engine's interned strings, which the names a held file is
    /// filed under join as opcache's buffer takes them ([`UnitCache::hold`],
    /// [`UnitCache::add_key`]).
    interned: crate::arena::Interned,
}

/// The keys of a unit's `function_table` and `class_table`: the lowered
/// name of each function its compile bound and of each class it bound
/// early, php's `lcname` (`zend_begin_func_decl`,
/// `zend_compile_class_decl`). A class a `DECLARE_CLASS` binds is held
/// under its runtime definition key, a constant of the unit already.
fn table_keys(unit: &Unit) -> Vec<Vec<u8>> {
    let mut early = vec![true; unit.classes.len()];
    for (_, keyed) in &unit.runtime_keys {
        if let Some(bound) = usize::try_from(*keyed)
            .ok()
            .and_then(|at| early.get_mut(at))
        {
            *bound = false;
        }
    }
    let functions = unit.functions.iter().map(|(decl, _)| &decl.name);
    let classes = (unit.classes.iter().zip(early))
        .filter(|(_, early)| *early)
        .map(|(decl, _)| &decl.name);
    functions
        .chain(classes)
        .map(|name| name.0.to_ascii_lowercase())
        .collect()
}

/// The files one epoch of the cache holds (design §5.14), what an
/// opcache restart empties by handing the next request a new epoch:
/// php's `ZCSG(hash)`, what `ZSMMG` counts of the memory under it,
/// `ZCSG(restart_pending)`, and the counters `zend_reset_cache_vars`
/// zeroes at the restart (`Z/ext/opcache/ZendAccelerator.c:2626`). A
/// file's early-bound classes are rows of its epoch's arena, so a request
/// finds only the files its own epoch held.
pub(crate) struct Files {
    held: Cache<FiledKey, Held>,
    /// How many names the files may be held under.
    capacity: usize,
    /// `ZCSG(hits)`: files served from the cache.
    hits: AtomicU64,
    /// `ZCSG(misses)`: files compiled for the cache.
    misses: AtomicU64,
    /// `ZCSG(blacklist_misses)`: files the blacklist kept out.
    blacklist_misses: AtomicU64,
    /// What the files held weigh, the preload's not among them.
    used: AtomicU64,
    /// `ZSMMG(wasted_shared_memory)`: what the files discarded in this
    /// epoch weighed, whose rows (the arena's among them) stand unused
    /// until the epoch is dropped.
    wasted: AtomicU64,
    /// `ZSMMG(memory_exhausted)`: a file found no room in this epoch, the
    /// memory or the names it is filed under full.
    exhausted: AtomicBool,
    /// `ZCSG(restart_pending)` with `ZCSG(restart_reason)`: the restart
    /// scheduled to replace this epoch, under the first schedule's reason.
    restart: OnceLock<Reason>,
    /// Each op array of every unit held in this epoch, with its safepoint
    /// words, kept until the epoch is dropped whatever replaces the file
    /// (`jit-design.md` §3.1; [`crate::tier::published`]).
    pub(crate) funcs: crate::tier::published::Funcs,
}

impl Files {
    /// No file held, under at most `capacity` names.
    pub(crate) fn new(capacity: usize) -> Files {
        Files {
            held: Cache::new(capacity),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            blacklist_misses: AtomicU64::new(0),
            used: AtomicU64::new(0),
            wasted: AtomicU64::new(0),
            exhausted: AtomicBool::new(false),
            restart: OnceLock::new(),
            funcs: crate::tier::published::Funcs::numbered_from(0),
        }
    }

    /// The files the next epoch starts with: none, under as many names,
    /// their op arrays numbered past this epoch's.
    pub(crate) fn emptied(&self) -> Files {
        Files {
            funcs: self.funcs.after(),
            ..Files::new(self.capacity)
        }
    }

    /// A hit on `held`, counted on the epoch and on the file, which was
    /// served at the request time `now`; php without the cache,
    /// [`Validation::EveryHit`], counts nothing.
    pub(crate) fn hit(&self, held: &Held, validation: Validation, now: i64) {
        if validation == Validation::EveryHit {
            return;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        held.hits.fetch_add(1, Ordering::Relaxed);
        held.last_used.store(now, Ordering::Relaxed);
    }

    /// `ZCSG(blacklist_misses)++`: a file the blacklist kept out of the
    /// cache was compiled for its request.
    pub(crate) fn note_blacklisted(&self) {
        self.blacklist_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// `ZCSG(restart_pending)`.
    pub(crate) fn restart_pending(&self) -> bool {
        self.restart.get().is_some()
    }

    /// `zend_accel_schedule_restart(ACCEL_RESTART_USER)`: this epoch
    /// replaced at the next request's start, and until then nothing new
    /// held in it; a second call while one is pending schedules nothing.
    pub(crate) fn schedule_restart(&self) -> Restart {
        self.schedule(Reason::User)
    }

    /// `zend_accel_schedule_restart` for `reason`, which a restart
    /// already pending keeps its own reason over. One scheduled for an
    /// epoch a restart has replaced already replaces nothing more.
    fn schedule(&self, reason: Reason) -> Restart {
        match self.restart.set(reason) {
            Ok(()) => Restart::Scheduled,
            Err(_) => Restart::Pending,
        }
    }
}

/// Where a file is looked for and kept, the extension's answer in the
/// cache's terms ([`crate::api::abi::Admission`]).
pub(crate) enum Levels<'a> {
    /// Memory alone.
    Memory,
    /// Memory, and on a miss the file cache, which keeps what is
    /// compiled too.
    Both(Disk<'a>),
    /// The file cache alone, `file_cache_compile_file`.
    Disk(Disk<'a>),
}

/// One file asked of [`UnitCache::file`]: its full path, the source a
/// `zend_compile_file` link handed down to compile in place of the
/// file's own where one did, the path as written it is filed under too
/// where given, how a hit is validated at the request time `now`, how
/// new a file is kept, and where, and what opcache's optimizer makes of
/// what it compiles; `opened` where the `zend_file_handle` holds the
/// file's opened path, by which the file cache reads it.
pub(crate) struct Asked<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) source: Option<&'a [u8]>,
    pub(crate) written: Option<UnitKey<'a>>,
    pub(crate) opened: bool,
    pub(crate) validation: Validation,
    pub(crate) protection: u32,
    pub(crate) now: i64,
    pub(crate) levels: Levels<'a>,
    pub(crate) optimizer: Optimizer,
    /// How the safepoint words of a unit held for this question are laid
    /// (`tier::Tier::laid`), as the tier answered for the request asking.
    pub(crate) laid: crate::tier::Laid,
    /// Set where the cache held a unit it compiled for this question, its
    /// words laid as `laid` says, and not where another request held the
    /// path's copy meanwhile ([`Holding::Found`]), nor for a unit the file
    /// cache read back, whose words are sealed as php's file cache
    /// restores op arrays the tier never saw.
    pub(crate) held: &'a std::cell::Cell<bool>,
}

/// What opcache's optimizer makes of a unit it persists that a request
/// reads, `zend_optimize_script` under `opcache.optimization_level`, which
/// `cache_script_in_shared_memory` and `cache_script_in_file_cache` call
/// on what they compiled (`Z/ext/opcache/ZendAccelerator.c:1581`,
/// `:1563`) and `preload_optimize` on what the preload holds (`:4425`,
/// `:4441`). The engine
/// runs none of its passes over what executes (design §6.1); of pass 11,
/// `zend_optimizer_compact_literals`, it keeps the runtime cache size the
/// pass counts, as the room each op array's cache takes of a request's
/// heap ([`php_unit::OpArray::cache_room`], [`compact`]); and of what
/// the passes lay on the request's heap as they work, the bytes
/// (`declared_bytes::optimized`). So where the accelerator holds a file,
/// sites naming one function or one class share one slot, and so do the
/// sites naming one constant, and one property or one method of `$this`
/// where the op array is sure of it, a method's or a closure's written
/// in one; a closure's own cache,
/// `emalloc`ed at that size, reads one bin however many it holds. A file
/// compiled without the accelerator keeps the compiler's slot per site.
/// Under the plain recipe each closure below takes one site of each
/// more than the one before:
///
/// ```php
/// $src = '<?php class S { function closures() { return [';
/// for ($n = 1; $n <= 4; $n++) { $src .= 'function () { ' . str_repeat('new stdClass; array_keys($x); FOO; $this->p; $this->m(); ', $n) . '}, '; }
/// file_put_contents(__DIR__ . '/sites.inc', $src . ']; } } return (new S)->closures();'); touch(__DIR__ . '/sites.inc', time() - 10);
/// class A {} $a = new A;
/// foreach (include __DIR__ . '/sites.inc' as $f) { $m = memory_get_usage(); $b = Closure::bind($f, $a, A::class); echo memory_get_usage() - $m, " "; unset($b); }
/// ```
/// ```expect
/// 448 512 576 640
/// ```
/// ```php
/// $src = '<?php class S { function closures() { return [';
/// for ($n = 1; $n <= 4; $n++) { $src .= 'function () { ' . str_repeat('new stdClass; array_keys($x); FOO; $this->p; $this->m(); ', $n) . '}, '; }
/// file_put_contents(__DIR__ . '/sites.inc', $src . ']; } } return (new S)->closures();'); touch(__DIR__ . '/sites.inc', time() - 10);
/// class A {} $a = new A;
/// foreach (include __DIR__ . '/sites.inc' as $f) { $m = memory_get_usage(); $b = Closure::bind($f, $a, A::class); echo memory_get_usage() - $m, " "; unset($b); }
/// ```
/// ```expect regime=immutable-literals
/// 448 448 448 448
/// ```
///
/// The count starts past the op array extensions' slots, the tier's
/// where it starts:
///
/// ```php
/// $src = '<?php class S { function closures() { return [';
/// for ($n = 1; $n <= 4; $n++) { $src .= 'function () { ' . str_repeat('new stdClass; array_keys($x); FOO; $this->p; $this->m(); ', $n) . '}, '; }
/// file_put_contents(__DIR__ . '/sites.inc', $src . ']; } } return (new S)->closures();'); touch(__DIR__ . '/sites.inc', time() - 10);
/// class A {} $a = new A;
/// foreach (include __DIR__ . '/sites.inc' as $f) { $m = memory_get_usage(); $b = Closure::bind($f, $a, A::class); echo memory_get_usage() - $m, " "; unset($b); }
/// ```
/// ```expect regime=jit-off
/// 464 464 464 464
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Optimizer {
    /// No `zend_optimize_script`: php compiling without the accelerator,
    /// or no extension declaring `opcache.optimization_level`.
    Off,
    /// `zend_optimize_script` under the passes `level` names, pass 11
    /// counting from `first`, the first slot past the op array
    /// extensions' (`zend_op_array_extension_handles * sizeof(void*)`).
    On { level: Level, first: u32 },
}

/// `opcache.optimization_level`: the passes `zend_optimize_script` runs,
/// one bit each (`Z/Zend/Optimizer/zend_optimizer.h:30-45`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Level(pub(crate) i64);

impl Level {
    /// `ZEND_OPTIMIZER_PASS_5`, the CFG pass.
    const PASS_5: i64 = 1 << 4;
    /// `ZEND_OPTIMIZER_PASS_6`, the DFA pass.
    const PASS_6: i64 = 1 << 5;
    /// `ZEND_OPTIMIZER_PASS_7`, the call graph.
    const PASS_7: i64 = 1 << 6;
    /// `ZEND_OPTIMIZER_PASS_11`, `zend_optimizer_compact_literals`.
    const PASS_11: i64 = 1 << 10;

    /// Whether the CFG pass runs, `zend_optimize_cfg`.
    pub(crate) fn cfg(self) -> bool {
        self.0 & Level::PASS_5 != 0
    }

    /// Whether the passes walk the call graph, every op array
    /// reverted before any is redone (`Z/Zend/Optimizer/zend_optimizer.c:1612`).
    pub(crate) fn call_graph(self) -> bool {
        self.0 & Level::PASS_6 != 0 && self.0 & Level::PASS_7 != 0
    }

    /// Whether pass 11 merges equal constants and counts the slots.
    pub(crate) fn compacts_literals(self) -> bool {
        self.0 & Level::PASS_11 != 0
    }
}

impl Optimizer {
    /// The passes on a miss `persistent_compile_file` compiled under
    /// `validation`: these with the accelerator on (every validation but
    /// `EveryHit`) where it optimizes what it compiled, `optimizing`,
    /// which with a restart pending it does only where a file cache keeps
    /// the file, as `file_cache_compile_file` does
    /// (`Z/ext/opcache/ZendAccelerator.c:2156-2163`); none where it
    /// compiled as php without the accelerator does.
    pub(crate) fn on_miss(self, validation: Validation, optimizing: bool) -> Optimizer {
        if validation == Validation::EveryHit || !optimizing {
            return Optimizer::Off;
        }
        self
    }

    /// What the passes leave of `unit`'s op arrays' caches, before the
    /// unit is published.
    pub(crate) fn optimized(self, mut unit: Unit) -> Unit {
        if let Optimizer::On { level, first } = self
            && level.compacts_literals()
        {
            for code in crate::exec::op_arrays_mut(&mut unit) {
                code.cache_room = compact::cache_size(code, first);
            }
        }
        unit
    }
}

/// The unit [`UnitCache::file`] answers with: one held, published for
/// every request with its map-pointer offsets, or one held nowhere, the
/// asking request's own (`Request::take_unit`), as php keeps a file it
/// does not persist in process memory with heap caches
/// (`zend_file_cache_unserialize_op_array`'s `script->corrupted` arm), so
/// `CG(map_ptr_last)` moves only for what is held (GH-8646).
pub(crate) enum Compiled {
    Published(PublishedUnit),
    Alone(Box<Unit>),
}

/// Whom a compile's answer serves: every request that asks for the file,
/// or the one that asked alone, whose `memory_limit` refused the compile
/// (`crate::engine::CompileHeap`), which another request with room for
/// it compiles again, as php compiles a file afresh where opcache cached
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answers {
    /// Any request: what a held unit answers.
    Every,
    /// The request that asked, and no other.
    Asker,
}

/// A compile [`UnitCache::file`] ran: the unit or the refusal, and whom
/// it answers.
pub(crate) struct Fresh {
    pub(crate) unit: Result<Unit, CompileError>,
    pub(crate) answers: Answers,
    /// The elements of the compile's tree, its nodes and significant
    /// tokens, which php's `CG(ast_arena)` is charged by.
    pub(crate) tree: usize,
}

/// What [`UnitCache::file`] answers: the unit, what it did with the
/// file, and the classes a held unit's compiler bound early.
pub(crate) type Answered = (
    Option<Result<Compiled, CompileError>>,
    Filed,
    Option<EarlyBound>,
);

/// Whether `unit`, compiled under `validation`, takes the names a
/// program reads from the request it was compiled for, as php without
/// the cache names them: [`Validation::EveryHit`] stands for php
/// compiling every include afresh, and an anonymous class's name is made
/// on from that request's `CG(rtd_key_counter)`, which the next include
/// or request has moved, so a unit declaring one is never held to be
/// served again. A unit declaring none reads the same however often it
/// is compiled, its keys binding named declarations alone, and is held;
/// its hit counts the keys php's compile would have made
/// ([`crate::api::request::Request::cached_source`]). Under opcache the
/// held unit keeps its miss's names, as `zend_persistent_script` does.
///
/// ```php
/// $inc = __DIR__ . '/anonymous.inc'; file_put_contents($inc, "<?php\nreturn new class {};\n"); touch($inc, time() - 10);
/// $n = fn ($o) => strstr(get_class($o), '$');
/// $a = include $inc;
/// echo $n($a), ' ', $n(eval('return new class {};')), ' ', $n($b = include $inc), ' ', $n(eval('return new class {};')), "\n";
/// var_dump($a instanceof $b);
/// ```
/// ```expect
/// $0 $1 $2 $3
/// bool(false)
/// ```
/// ```php
/// $inc = __DIR__ . '/anonymous.inc'; file_put_contents($inc, "<?php\nreturn new class {};\n"); touch($inc, time() - 10);
/// $n = fn ($o) => strstr(get_class($o), '$');
/// $a = include $inc;
/// echo $n($a), ' ', $n(eval('return new class {};')), ' ', $n($b = include $inc), ' ', $n(eval('return new class {};')), "\n";
/// var_dump($a instanceof $b);
/// ```
/// ```expect regime=jit-off
/// $0 $1 $0 $2
/// bool(true)
/// ```
///
/// A class declared in a block takes a key too, which the hit of a
/// unit with no anonymous class counts without the cache, as the
/// compile php makes of each include counts it, and counts nothing
/// under opcache:
///
/// ```php
/// $inc = __DIR__ . '/guarded.inc'; file_put_contents($inc, "<?php\nif (!class_exists('Guarded', false)) { class Guarded {} }\n"); touch($inc, time() - 10);
/// include $inc; include $inc;
/// echo strstr(get_class(eval('return new class {};')), '$'), "\n";
/// ```
/// ```expect
/// $2
/// ```
/// ```php
/// $inc = __DIR__ . '/guarded.inc'; file_put_contents($inc, "<?php\nif (!class_exists('Guarded', false)) { class Guarded {} }\n"); touch($inc, time() - 10);
/// include $inc; include $inc;
/// echo strstr(get_class(eval('return new class {};')), '$'), "\n";
/// ```
/// ```expect regime=jit-off
/// $1
/// ```
pub(crate) fn named_by_request(validation: Validation, unit: &Result<Unit, CompileError>) -> bool {
    validation == Validation::EveryHit
        && unit
            .as_ref()
            .is_ok_and(|unit| unit.classes.iter().any(|class| class.flags.is_anonymous))
}

/// The runtime definition keys and anonymous class names `unit`'s
/// compile made, which [`crate::api::request::Request::count_keys_made`]
/// counts on for the request it was compiled for, saturating at
/// `u32::MAX`.
pub(crate) fn keys_of(unit: &Unit) -> u32 {
    u32::try_from(unit.runtime_keys.len()).unwrap_or(u32::MAX)
}

/// The runtime definition keys a request counts for `unit` served as a
/// hit under `validation`: none under opcache, whose hit compiles
/// nothing; without it the keys php's compile of the include would
/// have made, as [`named_by_request`] reads [`Validation::EveryHit`].
pub(crate) fn hit_keys(validation: Validation, unit: &Unit) -> u32 {
    if validation == Validation::EveryHit {
        keys_of(unit)
    } else {
        0
    }
}

/// The unit `held` holds, answered as `filed` with its early-bound
/// classes.
fn answered_by(held: &Held, filed: Filed) -> Answered {
    let unit = Compiled::Published(held.unit.clone());
    (Some(Ok(unit)), filed, Some(Arc::clone(&held.early)))
}

/// A unit held nowhere, answered as `filed`.
fn alone(unit: Result<Unit, CompileError>, filed: Filed) -> Answered {
    (
        Some(unit.map(|unit| Compiled::Alone(Box::new(unit)))),
        filed,
        None,
    )
}

/// What [`UnitCache::file`] answered, as its caller serves it: a unit
/// held nowhere made `own`'s, and so what a refusal had declared, whose
/// strings are the engine's as a held unit's are.
pub(crate) fn served(
    answered: Option<Result<Compiled, CompileError>>,
    own: impl FnOnce(Unit) -> PublishedUnit,
) -> Cached {
    answered.map(|compiled| match compiled {
        Ok(Compiled::Published(unit)) => Ok(unit),
        Ok(Compiled::Alone(unit)) => Ok(own(*unit)),
        Err(error) => Err(CompileRefused::published(error, own)),
    })
}

/// What [`UnitCache::file`] did with the file it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filed {
    /// Served as held.
    Hit,
    /// Compiled and held, `cache_script_in_shared_memory`.
    Held,
    /// Compiled, and answered by the copy held under its full path
    /// already, which `cache_script_in_shared_memory` prefers to what it
    /// compiled (`Z/ext/opcache/ZendAccelerator.c:1588-1610`): neither a
    /// hit nor held anew.
    Existing,
    /// Held nowhere: read back from the file cache, or not read.
    Unheld,
    /// Compiled and kept in the file cache alone: under
    /// `opcache.file_cache_only`, or where a file cache is set and a
    /// restart is pending or memory is short (`cache_script_in_file_cache`,
    /// `file_cache_compile_file`); with no file cache those are
    /// [`Filed::Compiled`].
    Kept,
    /// Compiled and held nowhere, by `accelerator_orig_compile_file` as
    /// php without opcache compiles, whose classes bind early in place.
    Compiled,
    /// Neither read nor compiled: left to the asker's own compile,
    /// `accelerator_orig_compile_file`, a unit of its alone, as
    /// `persistent_compile_file` leaves a file where no room is left and
    /// `opcache_compile_file` one it does not keep.
    Own,
}

/// What [`UnitCache::hold`] did with a compile: held it, or found the
/// path's copy held already, which is kept.
enum Holding {
    Built(Arc<Held>),
    Found(Arc<Held>),
}

/// `ZCSG(preload_script)`'s name in the cache, the one pseudo-file the
/// preload's classes and functions are persisted as (`$PRELOAD$`,
/// `CG(compiled_filename)` in `accel_preload`).
pub(crate) const PRELOAD_SCRIPT: &[u8] = b"$PRELOAD$";

/// What the preload weighs in the cache: its pseudo-file and each file
/// it saved.
fn preload_weight(preloaded: &Preloaded) -> u64 {
    let saved = preloaded.scripts.iter().map(|held| held.size);
    saved.fold(preloaded.statistics.memory_consumption, u64::saturating_add)
}

/// The source a `zend_compile_file` link `handed` down, or else the bytes
/// of the file at `path`; none where it cannot be read.
pub(crate) fn source_or_read<'a>(handed: Option<&'a [u8]>, path: &[u8]) -> Option<Cow<'a, [u8]>> {
    if let Some(handed) = handed {
        return Some(Cow::Borrowed(handed));
    }
    let path = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(path);
    std::fs::read(path).ok().map(Cow::Owned)
}

/// Now, in whole seconds since the epoch, php's `time_t`.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}

impl UnitCache {
    /// The counters of a cache for this build over a bundle that
    /// declares `functions` and `classes`, under the tier's `hooked` and
    /// the host's `extended_info` ([`build_identity`]).
    pub(crate) fn new(
        hooked: php_compile::Hooked,
        extended_info: bool,
        functions: &[php_unit::decl::FunctionDecl],
        classes: &[php_unit::decl::ClassDecl],
        interned: crate::arena::Interned,
    ) -> UnitCache {
        let names = functions.iter().map(|decl| &*decl.name.0);
        let build = build_identity(
            hooked,
            extended_info,
            names.chain(classes.iter().map(|decl| &*decl.name.0)),
        );
        UnitCache {
            taken: AtomicU64::new(0),
            start_time: now(),
            last_restart_time: AtomicI64::new(0),
            manual_restarts: AtomicU64::new(0),
            oom_restarts: AtomicU64::new(0),
            hash_restarts: AtomicU64::new(0),
            budget: OnceLock::new(),
            preloaded: OnceLock::new(),
            build,
            interned,
        }
    }

    /// What the preload left, once it has been served.
    pub(crate) fn preloaded(&self) -> Option<&Preloaded> {
        self.preloaded.get()
    }

    /// The preload's tables and saved scripts published, once: the
    /// first publication stands.
    pub(crate) fn publish_preload(&self, preloaded: Preloaded) {
        self.preloaded.set(preloaded).ok();
    }

    /// The file `files` holds under `key`, a full path or a path as
    /// written: a saved file of the preload first, as php's hash holds
    /// them; none for a discarded one, which php's lookups pass over
    /// (`!persistent_script->corrupted`).
    pub(crate) fn find(&self, files: &Files, key: &UnitKey<'_>) -> Option<Arc<Held>> {
        if let Some(held) = self
            .preloaded()
            .and_then(|preloaded| preloaded.script(key.bytes()))
        {
            return Some(Arc::clone(held));
        }
        files.held.get(key.bytes()).filter(|held| !held.discarded())
    }

    /// `zend_accel_add_key`: `key` filed beside the names `held` is held
    /// under in `files`, where it names nothing yet, a discarded copy
    /// being something.
    pub(crate) fn add_key(&self, files: &Files, key: UnitKey<'_>, held: &Arc<Held>) {
        if files.held.get(key.bytes()).is_none() {
            self.update_key(files, key, held);
        }
    }

    /// `zend_accel_hash_update` of `key` to `held`'s entry: in place of
    /// what it names, or filed anew where the names are not full.
    fn update_key(&self, files: &Files, key: UnitKey<'_>, held: &Arc<Held>) {
        match files.held.get(key.bytes()) {
            Some(found) if Arc::ptr_eq(&found, held) => return,
            None if self.hash_full(files) => return,
            _ => {}
        }
        // `accel_new_interned_key`: the name as written is one of the
        // buffer's strings from here.
        php_heap::Intern::intern(&mut *self.interned.strings(), key.bytes()).ok();
        files.held.alias(key.filed(), held);
    }

    /// The first name `zend_accel_script_persist` interns of a unit it
    /// persists: its `filename`, the full path `path`, hashed as the
    /// engine's table takes it, which the [`Held`] file keeps as a value
    /// of that table and nothing a request owns; a null where the table
    /// has no room for it. A unit served without being
    /// held is persisted by nothing, and names none.
    pub(crate) fn name(&self, path: &[u8]) -> php_heap::FrozenZval {
        let mut strings = self.interned.strings();
        php_heap::FrozenZval::interned(&mut strings, path).unwrap_or(php_heap::FrozenZval::null())
    }

    /// The names `zend_accel_script_persist` interns beside the strings
    /// of a unit it persists, after its filename ([`UnitCache::name`]):
    /// the keys of its `function_table` and `class_table`
    /// ([`table_keys`]).
    pub(crate) fn persist_names(&self, unit: &Unit) {
        let mut strings = self.interned.strings();
        for key in table_keys(unit) {
            php_heap::Intern::intern(&mut *strings, &key).ok();
        }
    }

    /// The bounds the extension set, once: the first stands.
    pub(crate) fn set_budget(&self, budget: CacheBudget) {
        self.budget.set(budget).ok();
    }

    /// Whether the extension that bound the cache set the tier up in
    /// its segment ([`crate::api::request::CacheBudget::tier`]), which
    /// `zend_jit_check_support` may still refuse
    /// ([`crate::engine::Shared::op_array_extensions`]).
    pub(crate) fn tier_set_up(&self) -> bool {
        self.budget.get().is_some_and(|budget| budget.tier)
    }

    /// `zend_accel_hash_is_full`, which marks the memory exhausted and
    /// schedules a restart where enough is wasted.
    fn hash_full(&self, files: &Files) -> bool {
        let Some(budget) = self.budget.get() else {
            return false;
        };
        if files.held.len() < usize::try_from(budget.keys).unwrap_or(usize::MAX) {
            return false;
        }
        files.exhausted.store(true, Ordering::Relaxed);
        self.schedule_restart_if_necessary(files, Reason::Hash);
        true
    }

    /// Whether a file weighing `size` finds room: a name free in the
    /// hash, and `size` within what is neither used nor wasted, as
    /// `zend_shared_alloc_aligned` finds it; else the memory exhausted
    /// and a restart scheduled where enough is wasted, and the file is
    /// held nowhere.
    fn room_for(&self, files: &Files, size: u64) -> bool {
        if self.hash_full(files) {
            return false;
        }
        let Some(budget) = self.budget.get() else {
            return true;
        };
        let preload = self.preloaded().map_or(0, preload_weight);
        let taken = (files.used.load(Ordering::Relaxed))
            .saturating_add(files.wasted.load(Ordering::Relaxed))
            .saturating_add(preload);
        if budget.memory.saturating_sub(taken) >= size {
            return true;
        }
        files.exhausted.store(true, Ordering::Relaxed);
        self.schedule_restart_if_necessary(files, Reason::Oom);
        false
    }

    /// `zend_accel_discard_script`: `held` marked discarded where it is
    /// filed, as php marks it `corrupted` and leaves it in the hash, its
    /// weight counted as wasted once however many requests discard it,
    /// and where the memory is exhausted a restart scheduled if enough is
    /// now wasted.
    pub(crate) fn discard_held(&self, files: &Files, held: &Held) {
        if held.discarded.swap(true, Ordering::Relaxed) {
            return;
        }
        files.used.fetch_sub(
            held.size.min(files.used.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        files.wasted.fetch_add(held.size, Ordering::Relaxed);
        if files.exhausted.load(Ordering::Relaxed) {
            let reason = if self.hash_full(files) {
                Reason::Hash
            } else {
                Reason::Oom
            };
            self.schedule_restart_if_necessary(files, reason);
        }
    }

    /// `persistent_compile_file` by the file's full path: the file held
    /// under `path` where `validation` finds it fresh at the request time
    /// `now`, a hit; else the stale copy discarded, the file cache asked
    /// where there is one, a hit
    /// where it keeps the file as it stands, `decode`d and held under the
    /// full path alone; else a
    /// miss, the file read, or the source a link handed down taken in
    /// its place, and `compile`d, and what it compiled to held
    /// in place of a discarded copy, or what another request held for the
    /// path meanwhile, filed under
    /// `written` too where given, and kept in the file cache. A file
    /// whose stamp the validation or the `protection` needs and the
    /// system does not give, one written within `protection` seconds of
    /// `now`, and a miss while a restart is pending or the memory is
    /// exhausted with no file cache, is left to the caller's own compile
    /// ([`Filed::Own`]), as php compiles it without the cache; a unit
    /// [`named_by_request`] is held and kept nowhere; any other miss
    /// while a restart is pending is kept in the file cache alone.
    /// [`Levels::Disk`] asks and keeps in the file cache alone and counts
    /// nothing. Nothing where the file cannot be
    /// read. What is `compile`d or `decode`d takes no map-pointer offset
    /// until it is held, `share`d as it is; one held nowhere is answered
    /// [`Compiled::Alone`], its caller's own (GH-8646), and so is one
    /// that answers the request that asked alone ([`Answers::Asker`]).
    /// `compile` and `publish` are handed `caller`, the request the file
    /// is compiled and its classes published for, or nothing.
    #[allow(clippy::too_many_arguments)] // the table, the question, the caller and the four steps, each one fact
    #[allow(clippy::too_many_lines)] // `persistent_compile_file` and its two levels, in php's order
    pub(crate) fn file<A: Stat>(
        &self,
        caller: &mut A,
        files: &Files,
        asked: Asked<'_>,
        compile: impl FnOnce(&mut A, &[u8], Optimizer) -> Fresh,
        decode: impl FnOnce(&[u8]) -> Option<Unit>,
        share: impl FnOnce(Unit) -> PublishedUnit,
        publish: impl FnOnce(&mut A, &PublishedUnit) -> EarlyBound,
    ) -> Answered {
        let Asked {
            path,
            source: handed,
            written,
            opened,
            validation,
            protection,
            now,
            levels,
            optimizer,
            laid,
            held: held_now,
        } = asked;
        let (memory, disk) = match levels {
            Levels::Memory => (true, None),
            Levels::Both(disk) => (true, Some(disk)),
            Levels::Disk(disk) => (false, Some(disk)),
        };
        // A key that names a discarded copy is a miss that looks no full
        // path up, and a handle holding the path alone leaves no opened
        // path for the file cache to read the file by, as
        // `do_validate_timestamps` gives its resolved path back
        // (`Z/ext/opcache/ZendAccelerator.c:1157-1160`, `zend_file_cache.c:1901`).
        let keyed = (written.as_ref()).is_some_and(|key| {
            files
                .held
                .get(key.bytes())
                .is_some_and(|held| held.discarded())
        });
        if memory
            && !keyed
            && let Some(hit) = self.held_hit(caller, files, path, written.as_ref(), validation, now)
        {
            return answered_by(&hit, Filed::Hit);
        }
        let validated = validation != Validation::Never;
        let stamped = validated || protection > 0;
        let observed = if stamped { caller.stamp(path) } else { None };
        // The second level, `zend_file_cache_script_load`: read back
        // where it keeps the file at the `mtime` it has now, where
        // timestamps are validated.
        if let Some(disk) = disk.as_ref().filter(|_| opened || !keyed)
            && (!validated || observed.is_some())
            && let Some(kept) = disk.load(path, observed.filter(|_| validated).map(Stamp::seconds))
            && let Some(unit) = decode(&kept.body)
        {
            // Held as `zend_file_cache_script_load` puts it in the shared
            // hash, a hit, except while a restart is pending.
            if !memory {
                return alone(Ok(unit), Filed::Unheld);
            }
            if files.restart_pending() || !self.room_for(files, kept.size) {
                files.hits.fetch_add(1, Ordering::Relaxed);
                return alone(Ok(unit), Filed::Unheld);
            }
            // Under its full path alone: `zend_file_cache_script_load`
            // files no key, which the next include adds after opening it.
            // Its words are sealed whatever the request's tier says:
            // `zend_file_cache_script_load_ex` restores each op array's
            // handlers as the file keeps them and never reaches
            // `zend_jit_op_array`, which only the persist after a compile
            // calls (`Z/ext/opcache/zend_file_cache.c:1901-2115`,
            // `Z/ext/opcache/zend_persist.c:771-775`, `:1524-1525`), so
            // the unit counts nothing and hands nothing off (#3362, #3363).
            let unit = share(unit);
            let early = publish(caller, &unit);
            let read = Read {
                unit,
                early,
                size: kept.size,
                observed,
            };
            let sealed = (path, crate::tier::Laid::Sealed);
            let (Holding::Built(held) | Holding::Found(held)) =
                self.hold(files, sealed, validation, now, read);
            files.hit(&held, validation, now);
            return answered_by(&held, Filed::Hit);
        }
        let accelerated = memory && validation != Validation::EveryHit;
        if accelerated {
            files.misses.fetch_add(1, Ordering::Relaxed);
        }
        // `accelerator_orig_compile_file`, the request's own compile, where
        // there is no room and no file cache to keep it in, and for a file
        // with no stamp or one written within `protection` seconds of
        // `now`, which `opcache_compile_file` decides before it compiles.
        let short = accelerated
            && disk.is_none()
            && (files.restart_pending() || files.exhausted.load(Ordering::Relaxed));
        let unheld = stamped
            && observed.is_none_or(|stamp| {
                protection > 0 && now.saturating_sub(i64::from(protection)) < stamp.seconds()
            });
        if short || unheld {
            return (None, Filed::Own, None);
        }
        let Some(source) = source_or_read(handed, path) else {
            return (None, Filed::Unheld, None);
        };
        // What `cache_script_in_shared_memory` and
        // `cache_script_in_file_cache` will run over what compiles.
        let optimizer = optimizer.on_miss(validation, disk.is_some() || !files.restart_pending());
        let Fresh { unit, answers, .. } = compile(caller, &source, optimizer);
        let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
        if answers == Answers::Asker || named_by_request(validation, &unit) {
            return alone(unit, Filed::Compiled);
        }
        // php persists no refusal, so its corrupted copy stays filed.
        let Ok(unit) = unit else {
            return alone(unit, Filed::Compiled);
        };
        let unit = optimizer.optimized(unit);
        // `cache_script_in_shared_memory` looks for the path's copy before
        // it asks for room or writes the file cache.
        if memory
            && !files.restart_pending()
            && let Some(existing) = self.find(files, &UnitKey::full(path))
        {
            return self.existing(files, &existing, written, validation, observed, now);
        }
        if let Some(disk) = &disk
            && let Some(body) = wire::encode(&unit)
        {
            let stamp = observed.filter(|_| validated).map_or(0, Stamp::seconds);
            disk.store(path, stamp, size, &body);
        }
        if !memory || files.restart_pending() || !self.room_for(files, size) {
            return alone(Ok(unit), disk.map_or(Filed::Compiled, |_| Filed::Kept));
        }
        let unit = share(unit);
        let early = publish(caller, &unit);
        let read = Read {
            unit,
            early,
            size,
            observed,
        };
        let held = match self.hold(files, (path, laid), validation, now, read) {
            Holding::Built(held) => {
                held_now.set(true);
                held
            }
            Holding::Found(existing) => {
                return self.existing(files, &existing, written, validation, observed, now);
            }
        };
        // Not a `phar://` name, which may hold an alias the archive
        // keeps for the request alone (`ZendAccelerator.c:1669-1672`).
        if let Some(written) = written.filter(|key| !key.bytes().starts_with(b"phar://")) {
            self.update_key(files, written, &held);
        }
        answered_by(&held, Filed::Held)
    }

    /// `cache_script_in_shared_memory`'s `existing_persistent_script`
    /// (`Z/ext/opcache/ZendAccelerator.c:1588-1610`): `held`, the copy
    /// the path's compile found held already, a preloaded one among them,
    /// served at `now` in place of what was compiled, and filed under
    /// `written` where that names nothing and `validation` reads no stamp
    /// or the one `observed` as the compile's. It is looked for before
    /// the cache is asked for room, so a key retargeted at a held file
    /// leaves a cache filled to its last name as full as it was. Served by
    /// the binary, since the child is `PHP_BINARY`:
    ///
    /// ```php
    /// $d = realpath(sys_get_temp_dir()) . '/full' . getmypid(); @mkdir("$d/x", 0777, true); @mkdir("$d/y"); @mkdir("$d/n");
    /// foreach (['x', 'y'] as $t) { file_put_contents("$d/$t/a.inc", "<?php return '$t';"); touch("$d/$t/a.inc", time() - 60); }
    /// for ($i = 0; $i < 300; $i++) { file_put_contents("$d/n/$i.inc", '<?php return 0;'); touch("$d/n/$i.inc", time() - 60); }
    /// symlink("$d/x", "$d/to");
    /// $code = '$d = $argv[1]; $s = fn () => opcache_get_status(false); echo include "$d/to/a.inc", include "$d/y/a.inc";
    /// for ($i = 0; $s()["opcache_statistics"]["num_cached_keys"] < $s()["opcache_statistics"]["max_cached_keys"]; $i++) include "$d/n/$i.inc";
    /// var_dump($s()["cache_full"]); unlink("$d/to"); symlink("$d/y", "$d/to"); echo include "$d/to/a.inc", "\n"; var_dump($s()["cache_full"], $s()["restart_pending"]);';
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.revalidate_freq=0', '-d', 'opcache.max_accelerated_files=200', '-r', $code, $d], [1 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]); proc_close($p);
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect regime=jit-off process
    /// xybool(false)
    /// y
    /// bool(false)
    /// bool(false)
    /// ```
    fn existing(
        &self,
        files: &Files,
        held: &Arc<Held>,
        written: Option<UnitKey<'_>>,
        validation: Validation,
        observed: Option<Stamp>,
        now: i64,
    ) -> Answered {
        held.last_used.store(now, Ordering::Relaxed);
        if let Some(written) = written
            && (validation == Validation::Never
                || held.stamp.map(Stamp::seconds) == observed.map(Stamp::seconds))
        {
            self.add_key(files, written, held);
        }
        answered_by(held, Filed::Existing)
    }

    /// The first level's hit on `path`: a file the preload saved,
    /// whose timestamp php keeps at zero, served as held without the
    /// file looked at, whatever the window, and a held file `validation`
    /// finds fresh at `now`; each filed under `written` too. A stale copy
    /// is discarded, and a discarded one is a miss.
    fn held_hit(
        &self,
        stat: &mut dyn Stat,
        files: &Files,
        path: &[u8],
        written: Option<&UnitKey<'_>>,
        validation: Validation,
        now: i64,
    ) -> Option<Arc<Held>> {
        // The cache bypassed, `EveryHit`, compiles afresh.
        if validation != Validation::EveryHit
            && let Some(held) = self
                .preloaded()
                .and_then(|preloaded| preloaded.script(path))
        {
            files.hit(held, validation, now);
            if let Some(written) = written {
                self.add_key(files, written.clone(), held);
            }
            return Some(Arc::clone(held));
        }
        let stale = files.held.get(path).filter(|held| !held.discarded())?;
        if stale.validated(validation, now, stat) {
            files.hit(&stale, validation, now);
            if let Some(written) = written {
                self.add_key(files, written.clone(), &stale);
            }
            return Some(stale);
        }
        self.discard_held(files, &stale);
        None
    }

    /// What `read` compiled from the file at `path`, held in `files`
    /// under its full path, in place of a discarded copy under every name
    /// that copy was filed under, as `cache_script_in_shared_memory`
    /// updates the bucket every key of it points to; the copy another
    /// request held for the path meanwhile where it did; its op arrays'
    /// words `laid` as the tier answered (`tier::Tier::laid`).
    fn hold(
        &self,
        files: &Files,
        (path, laid): (&[u8], crate::tier::Laid),
        validation: Validation,
        now: i64,
        read: Read,
    ) -> Holding {
        let Read {
            unit,
            early,
            size,
            observed,
        } = read;
        let stamp = match validation {
            Validation::Never => None,
            Validation::Window { .. } | Validation::EveryHit => observed,
        };
        let revalidate = match validation {
            Validation::Window { seconds } => now.saturating_add(i64::from(seconds)),
            Validation::Never | Validation::EveryHit => 0,
        };
        // Interned before the file is held, so its string is hashed before
        // any request can read it; a copy another request held meanwhile
        // had it interned by that request already.
        let name = self.name(path);
        let mut built = false;
        let key = UnitKey::full(path).filed();
        let held = files.held.insert_over(key, Held::discarded, |replaced| {
            built = true;
            // `zend_accel_hash_update` puts the file in a discarded copy's
            // entry, where that stands in its bucket's chain.
            let next = || self.taken.fetch_add(1, Ordering::Relaxed);
            Held {
                unit,
                early,
                path: path.to_vec(),
                name,
                stamp,
                size,
                order: replaced.map_or_else(next, |was| was.order),
                hits: AtomicU64::new(0),
                last_used: AtomicI64::new(now),
                revalidate: AtomicI64::new(revalidate),
                discarded: AtomicBool::new(false),
            }
        });
        if !built {
            return Holding::Found(held);
        }
        files.used.fetch_add(size, Ordering::Relaxed);
        files.funcs.publish(&held.unit, laid);
        self.persist_names(&held.unit);
        Holding::Built(held)
    }

    /// `zend_accel_invalidate`'s discard of the file held under `key`:
    /// where `force`, and otherwise where it changed since,
    /// `do_validate_timestamps` finding it filed under another full path,
    /// held with no stamp, or written at another second. Whether a unit
    /// was held under the name at all; a copy discarded already is none.
    pub(crate) fn discard(
        &self,
        files: &Files,
        key: &UnitKey<'_>,
        force: bool,
        stat: &mut dyn Stat,
    ) -> bool {
        let Some(held) = files.held.get(key.bytes()).filter(|held| !held.discarded()) else {
            return false;
        };
        let changed = force
            || key.bytes() != held.path
            || held.stamp.is_none_or(|was| {
                stat.stamp(&held.path).map(Stamp::seconds) != Some(was.seconds())
            });
        if changed {
            self.discard_held(files, &held);
        }
        true
    }

    /// `zend_accel_schedule_restart_if_necessary`: a restart for `reason`
    /// where what is wasted in `files` reaches
    /// `opcache.max_wasted_percentage` of `opcache.memory_consumption`.
    fn schedule_restart_if_necessary(&self, files: &Files, reason: Reason) {
        let Some(budget) = self.budget.get() else {
            return;
        };
        let wasted = files.wasted.load(Ordering::Relaxed);
        let reached = u128::from(wasted).saturating_mul(100)
            >= u128::from(budget.memory).saturating_mul(u128::from(budget.max_wasted));
        if reached {
            files.schedule(reason);
        }
    }

    /// `accel_activate`'s restart, where one is pending for the epoch
    /// `files` are held in: counted under its reason and the time
    /// stamped, one second past the last where two fall in one second.
    /// Whether one was; the caller replaces the epoch, which empties the
    /// cache with nothing wasted or exhausted any more
    /// (`zend_shared_alloc_restore_state`) and nothing hit, missed or kept
    /// out yet (`zend_reset_cache_vars`), the preload's scripts kept as
    /// php keeps them ([`crate::arena::Epochs::enter`]).
    pub(crate) fn restart(&self, files: &Files) -> bool {
        let Some(reason) = files.restart.get() else {
            return false;
        };
        let counted = match reason {
            Reason::Oom => &self.oom_restarts,
            Reason::Hash => &self.hash_restarts,
            Reason::User => &self.manual_restarts,
        };
        counted.fetch_add(1, Ordering::Relaxed);
        let now = now();
        let last = self.last_restart_time.load(Ordering::Relaxed);
        let stamped = if last < now {
            now
        } else {
            last.saturating_add(1)
        };
        self.last_restart_time.store(stamped, Ordering::Relaxed);
        true
    }

    /// What `opcache_get_status()` reads in the epoch `files` are held
    /// in: the counters, the names held, and every file held under its
    /// full path, in `accelerator_get_scripts`' order
    /// (`Z/ext/opcache/zend_accelerator_module.c:634-635`), bucket by
    /// bucket of `ZCSG(hash)` and each bucket's chain newest first, a
    /// name's bucket `(zend_string_hash_val(name) ^ ZCG(root_hash)) %
    /// max_num_entries` (`Z/ext/opcache/zend_accelerator_hash.c:88-92`)
    /// and the preload's names the first in.
    ///
    /// ```php
    /// function bucket($s, $p) {
    ///     $hi = 0; $lo = 5381;
    ///     foreach (unpack('C*', $s) as $c) { $lo = $lo * 33 + $c; $hi = ($hi * 33 + ($lo >> 32)) & 0xffffffff; $lo &= 0xffffffff; }
    ///     return ((($hi | 0x80000000) % $p) * ((1 << 32) % $p) + $lo) % $p;
    /// }
    /// $p = current(array_filter([5, 11, 19, 53, 107, 223, 463, 983, 1979, 3907, 7963, 16229, 32531, 65407, 130987, 262237, 524521, 1048793], fn ($q) => $q >= ini_get('opcache.max_accelerated_files')));
    /// $d = realpath(__DIR__) . '/order' . getmypid(); @mkdir($d);
    /// $seen = [];
    /// for ($i = 0; ; $i++) {
    ///     $b = bucket("$d/n$i.inc", $p);
    ///     if ($b == $p - 1) continue;
    ///     if (isset($seen[$b])) { [$a, $bb] = ["$d/n{$seen[$b]}.inc", "$d/n$i.inc"]; break; }
    ///     $seen[$b] = $i;
    /// }
    /// for ($j = $i + 1; bucket("$d/n$j.inc", $p) <= $b; $j++);
    /// $c = "$d/n$j.inc";
    /// $name = [$a => 'A', $bb => 'B', $c => 'C'];
    /// foreach ($name as $f => $_) { file_put_contents($f, '<?php return 1;'); touch($f, time() - 60); }
    /// $listed = fn () => implode(' ', array_map(fn ($f) => $name[$f], array_values(array_filter(array_keys(opcache_get_status(true)['scripts']), fn ($f) => isset($name[$f])))));
    /// include $c; include $a; include $bb;
    /// echo $listed(), "\n";
    /// var_dump(opcache_invalidate($a, true));
    /// echo $listed(), "\n";
    /// include $a;
    /// echo $listed(), "\n";
    /// array_map('unlink', array_keys($name)); rmdir($d);
    /// ```
    /// ```expect regime=jit-off
    /// B A C
    /// bool(true)
    /// B A C
    /// B A C
    /// ```
    ///
    /// A discarded copy whose recompile is refused stays listed, its
    /// timestamp zeroed, as php persists no refusal in its place, and each
    /// include of the file is a miss:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/refused.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// include $f; include $f;
    /// var_dump(opcache_invalidate($f, true));
    /// file_put_contents($f, '<?php return ('); touch($f, time() - 30);
    /// $s = fn () => array_values(array_intersect_key(opcache_get_status()['opcache_statistics'], ['hits' => 0, 'misses' => 0]));
    /// [$h, $m] = $s();
    /// foreach ([1, 2] as $_) { try { include $f; } catch (ParseError $e) { echo get_class($e), "\n"; } }
    /// [$h2, $m2] = $s();
    /// $listed = opcache_get_status(true)['scripts'][$f];
    /// echo $h2 - $h, ' ', $m2 - $m, ' ', $listed['hits'], ' ', $listed['timestamp'], "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// bool(true)
    /// ParseError
    /// ParseError
    /// 0 2 1 0
    /// ```
    pub(crate) fn status(
        &self,
        files: &Files,
        interned_strings: usize,
        tier: TierStatus,
    ) -> CacheStatus {
        let mut held = files.held.held();
        held.retain(|(_, held)| {
            self.preloaded()
                .is_none_or(|preloaded| preloaded.script(&held.path).is_none())
        });
        let mut keys = u64::try_from(held.len()).unwrap_or(u64::MAX);
        held.retain(|(key, held)| key.bytes() == held.path);
        held.sort_by_key(|(_, held)| held.order);
        let listed = |held: &Held| CachedFile {
            path: held.path.clone(),
            hits: held.hits.load(Ordering::Relaxed),
            size: held.size,
            last_used: held.last_used.load(Ordering::Relaxed),
            // `zend_accel_discard_script` zeroes a discarded one's.
            timestamp: (held.stamp)
                .filter(|_| !held.discarded())
                .map_or(0, Stamp::seconds),
            revalidate: held.revalidate.load(Ordering::Relaxed),
        };
        // The preload's pseudo-file and its saved scripts stand in the
        // hash beside what the requests cached, never validated.
        let mut listed_files = Vec::new();
        if let Some(preloaded) = self.preloaded() {
            listed_files.push(CachedFile {
                path: PRELOAD_SCRIPT.to_vec(),
                hits: 0,
                size: preloaded.statistics.memory_consumption,
                last_used: 0,
                timestamp: 0,
                revalidate: 0,
            });
            listed_files.extend(preloaded.scripts.iter().map(|held| listed(held)));
            let preloaded = u64::try_from(listed_files.len()).unwrap_or(u64::MAX);
            keys = keys.saturating_add(preloaded);
        }
        let live = held.iter().filter(|(_, held)| !held.discarded());
        let used = (live.map(|(_, held)| held.size)).fold(
            self.preloaded().map_or(0, preload_weight),
            u64::saturating_add,
        );
        listed_files.extend(held.into_iter().map(|(_, held)| listed(&held)));
        // Each file's index here is the order its name entered the hash,
        // which no budget told leaves as it is.
        let mut listed_files: Vec<_> = listed_files.into_iter().enumerate().collect();
        if let Some(&CacheBudget { keys, root, .. }) = self.budget.get() {
            listed_files.sort_by_key(|(entered, file)| {
                let bucket = (php_heap::Str::hash_of(&file.path) ^ root) % keys.max(1);
                (bucket, std::cmp::Reverse(*entered))
            });
        }
        CacheStatus {
            restart_pending: files.restart_pending(),
            hits: files.hits.load(Ordering::Relaxed),
            misses: files.misses.load(Ordering::Relaxed),
            blacklist_misses: files.blacklist_misses.load(Ordering::Relaxed),
            start_time: self.start_time,
            last_restart_time: self.last_restart_time.load(Ordering::Relaxed),
            manual_restarts: self.manual_restarts.load(Ordering::Relaxed),
            oom_restarts: self.oom_restarts.load(Ordering::Relaxed),
            hash_restarts: self.hash_restarts.load(Ordering::Relaxed),
            used,
            wasted: files.wasted.load(Ordering::Relaxed),
            cache_full: files.exhausted.load(Ordering::Relaxed),
            interned_strings: u64::try_from(interned_strings).unwrap_or(u64::MAX),
            keys,
            files: listed_files.into_iter().map(|(_, file)| file).collect(),
            preload: self
                .preloaded()
                .map(|preloaded| preloaded.statistics.clone()),
            tier,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Borrow;
    use std::hash::BuildHasher;

    use super::{Cache, FiledKey, UnitKey};

    /// A name as written, borrowed for a lookup, and the same name filed
    /// are one name: each hashes and compares as its bytes, so the cache
    /// finds a filed name by the bytes of the name it was filed from, a
    /// full path and a relative one with what resolves it alike.
    #[test]
    fn a_filed_name_hashes_and_compares_as_the_name_it_was_filed_from() {
        let names = [
            UnitKey::full(b"/srv/app/index.php"),
            UnitKey::relative(
                b"lib.php",
                b"/srv/app",
                b".:/usr/share/php",
                Some(b"/srv/app/index.php"),
            ),
        ];
        let hashing = std::hash::RandomState::new();
        let cache = Cache::<FiledKey, usize>::new(names.len());
        for (at, name) in names.iter().enumerate() {
            let filed = name.clone().filed();
            assert_eq!(filed.bytes(), name.bytes());
            assert_eq!(Borrow::<[u8]>::borrow(&filed), Borrow::<[u8]>::borrow(name));
            assert_eq!(hashing.hash_one(&filed), hashing.hash_one(name.bytes()));
            assert_eq!(hashing.hash_one(name), hashing.hash_one(name.bytes()));
            cache.get_or_build(filed, |_| at);
        }
        for (at, name) in names.iter().enumerate() {
            assert_eq!(cache.get(name.bytes()).as_deref(), Some(&at));
        }
        assert_eq!(cache.get(&b"/srv/app/lib.php"[..]), None);
    }
}

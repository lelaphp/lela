//! The unit cache (design §6.3): what a file compiled to, held for every
//! request that asks for the same file, over the engine's one [`Cache`]
//! (ADR-0005), keyed by path and validated as the one question it asks
//! an extension answers (revision 45, [`crate::api::abi::Admission`]),
//! with the counters opcache keeps beside its shared hash. php's
//! `zend_persistent_script` is [`Held`], its `dynamic_members` the hits,
//! the last use and the next revalidation on it; `ZCSG(hits)`,
//! `ZCSG(misses)`, `ZCSG(blacklist_misses)`, the restart counters and
//! `ZCSG(restart_pending)` are [`UnitCache`]'s. What reads them is the
//! request's face, [`crate::api::request::UnitCache`], which `opcache_*`
//! answer from.

mod disk;
mod wire;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use php_compile::CompileError;
use php_unit::Unit;

use crate::api::abi::{Cache, Validation};
use crate::api::request::{CacheBudget, CacheStatus, CachedFile, Preloaded, Restart};

use disk::build_identity;
pub(crate) use disk::{BUILD, Disk};
pub(crate) use wire::decode;

/// What the unit cache answers for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read.
pub(crate) type Cached = Option<Result<Arc<Unit>, CompileError>>;

/// The classes a held unit's compiler bound early, each published into
/// the engine's arena as the unit was held (design §5.14, §6.3), by its
/// declaration's index: none for a class the unit leaves to an opline
/// and for one the arena could not take ([`crate::api::request::Request::publish_unit`]).
pub(crate) type EarlyBound = Arc<[Option<crate::api::values::ClassId>]>;

/// What the unit cache files a held file under, one space of names as
/// php's `ZCSG(hash)` is (design §6.3, revision 45): the file's full
/// path, and beside it the path as written, `accel_make_persistent_key`.
/// The file's stamp is on the [`Held`] file and not in the name, so one
/// path holds one file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UnitKey(Vec<u8>);

impl UnitKey {
    /// The name of the file at the full path `path`, which is also an
    /// absolute path as written.
    pub(crate) fn full(path: &[u8]) -> UnitKey {
        UnitKey(path.to_vec())
    }

    /// `accel_make_persistent_key` for a relative path as written: the
    /// path with what resolves it beside it, the working directory `cwd`,
    /// the `include_path` and the directory of the file `including` it,
    /// each apart by a byte no path holds.
    pub(crate) fn relative(
        written: &[u8],
        cwd: &[u8],
        include_path: &[u8],
        including: Option<&[u8]>,
    ) -> UnitKey {
        let mut key = [written, cwd, include_path].join(&0);
        if let Some(file) = including {
            let dir = file
                .iter()
                .rposition(|b| *b == b'/')
                .map_or(&[][..], |at| &file[..at]);
            key.push(0);
            key.extend_from_slice(dir);
        }
        UnitKey(key)
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

/// The `mtime` of the file at `path` in whole seconds, php's
/// `zend_get_file_handle_timestamp`; none where nothing is there.
pub(crate) fn stamp_seconds(path: &[u8]) -> Option<i64> {
    Stamp::of(path).map(Stamp::seconds)
}

/// What the system reported of a file as it was compiled, the
/// `zend_stat_t` php's `validate_timestamps` reads: when it was last
/// written and its size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    modified: Duration,
    size: u64,
}

impl Stamp {
    /// The file at `path` as the system reports it now; none where
    /// nothing is there or the system keeps no time for it.
    pub(crate) fn of(path: &[u8]) -> Option<Stamp> {
        let described = std::fs::metadata(String::from_utf8_lossy(path).as_ref()).ok()?;
        let modified = described.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
        Some(Stamp {
            modified,
            size: described.len(),
        })
    }

    /// `st_mtime`, the whole seconds php compares and never the size.
    fn seconds(self) -> i64 {
        i64::try_from(self.modified.as_secs()).unwrap_or(i64::MAX)
    }
}

/// One file the cache holds, php's `zend_persistent_script`: what it
/// compiled to, the path it was compiled from and the stamp it was
/// compiled at, and its `dynamic_members`, the hits since it was cached,
/// when it was last served and when it is next validated. The unit is
/// never changed; the members are atomics a racing request may write
/// over, which costs at most one more stat.
pub(crate) struct Held {
    pub(crate) unit: Result<Arc<Unit>, CompileError>,
    /// Its early-bound classes, published as it was held, which a
    /// request loading the unit declares by their numbers.
    pub(crate) early: EarlyBound,
    /// The full path, php's `script.filename`.
    pub(crate) path: Vec<u8>,
    /// The file as it was compiled; none for one held under
    /// [`Validation::Never`], php's zero timestamp, which a window never
    /// validates and nothing else finds fresh.
    stamp: Option<Stamp>,
    /// The bytes it was compiled from.
    size: u64,
    /// The order the cache took it in, which its status lists it by.
    order: u64,
    hits: AtomicU64,
    last_used: AtomicI64,
    /// `dynamic_members.revalidate`: the request time up to which a hit
    /// under a window is served without a stat.
    revalidate: AtomicI64,
    /// `corrupted`: discarded, its weight counted as wasted once.
    discarded: AtomicBool,
}

/// What a file read from disk came to, for [`UnitCache::hold`]: the
/// unit or the compiler's refusal, its early-bound classes published,
/// the bytes it was read from and its stamp as observed.
struct Read {
    unit: Result<Arc<Unit>, CompileError>,
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
    pub(crate) fn preloaded(unit: Arc<Unit>, path: Vec<u8>, size: u64, order: u64) -> Held {
        Held {
            unit: Ok(unit),
            early: Arc::new([]),
            path,
            stamp: None,
            size,
            order,
            hits: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            revalidate: AtomicI64::new(0),
            discarded: AtomicBool::new(false),
        }
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

    /// Whether a hit under `validation` at `now` is served without a
    /// stat.
    pub(crate) fn trusted(&self, validation: Validation, now: i64) -> bool {
        matches!(self.due(validation, now), Due::Nothing)
    }

    /// `validate_timestamp_and_record`: whether a hit under `validation`
    /// at `now` is served, the file as it stands stat'ed only where a
    /// hit is due, and the next revalidation time recorded where a
    /// window's stat finds the file unchanged.
    pub(crate) fn validated(&self, validation: Validation, now: i64) -> bool {
        match self.due(validation, now) {
            Due::Nothing => true,
            Due::Seconds { was, next } => {
                let fresh = Stamp::of(&self.path).is_some_and(|stamp| stamp.seconds() == was);
                if fresh {
                    self.revalidate.store(next, Ordering::Relaxed);
                }
                fresh
            }
            Due::Exact(held) => held.is_some() && Stamp::of(&self.path) == held,
        }
    }
}

/// `zend_accel_restart_reason`: why a restart was scheduled, which of
/// `ZCSG(oom_restarts)`, `ZCSG(hash_restarts)` and
/// `ZCSG(manual_restarts)` it counts under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Reason {
    /// `ACCEL_RESTART_OOM`: the memory exhausted.
    Oom,
    /// `ACCEL_RESTART_HASH`: the names a file is filed under full.
    Hash,
    /// `ACCEL_RESTART_USER`: `opcache_reset`.
    User,
}

impl Reason {
    fn of(byte: u8) -> Reason {
        match byte {
            0 => Reason::Oom,
            1 => Reason::Hash,
            _ => Reason::User,
        }
    }
}

/// The unit cache and opcache's counters over it.
pub(crate) struct UnitCache {
    held: Cache<UnitKey, Held>,
    taken: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    blacklist_misses: AtomicU64,
    /// `ZCSG(start_time)`: when the engine was built.
    start_time: i64,
    last_restart_time: AtomicI64,
    manual_restarts: AtomicU64,
    oom_restarts: AtomicU64,
    hash_restarts: AtomicU64,
    restart_pending: AtomicBool,
    /// `ZCSG(restart_reason)`: why the pending restart was scheduled,
    /// the first schedule's ([`Reason`]).
    restart_reason: AtomicU8,
    /// What the files held weigh, the preload's not among them.
    used: AtomicU64,
    /// `ZSMMG(wasted_shared_memory)`: what the files discarded since the
    /// last restart weighed, whose rows (the arena's among them) stand
    /// unused until a restart takes them back.
    wasted: AtomicU64,
    /// `ZSMMG(memory_exhausted)`: a file found no room since the last
    /// restart, the memory or the names it is filed under full.
    exhausted: AtomicBool,
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

/// One file asked of [`UnitCache::file`]: its full path, the path as
/// written it is filed under too where given, how a hit is validated at
/// the request time `now`, how new a file is kept, and where.
pub(crate) struct Asked<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) written: Option<UnitKey>,
    pub(crate) validation: Validation,
    pub(crate) protection: u32,
    pub(crate) now: i64,
    pub(crate) levels: Levels<'a>,
}

/// What [`UnitCache::file`] did with the file it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Filed {
    /// Served as held.
    Hit,
    /// Compiled and held, `cache_script_in_shared_memory`.
    Held,
    /// Held nowhere: read back from the file cache, or compiled and kept
    /// there alone, `file_cache_compile_file`.
    Unheld,
    /// Compiled and held nowhere, by `accelerator_orig_compile_file` as
    /// php without opcache compiles, whose classes bind early in place.
    Compiled,
}

/// `ZCSG(preload_script)`'s name in the cache, the one pseudo-file the
/// preload's classes and functions are persisted as (`$PRELOAD$`,
/// `CG(compiled_filename)` in `accel_preload`).
pub(crate) const PRELOAD_SCRIPT: &[u8] = b"$PRELOAD$";

/// Now, in whole seconds since the epoch, php's `time_t`.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}

impl UnitCache {
    /// An empty cache holding at most `capacity` names, for this build
    /// over a bundle that declares `functions` and `classes`, under the
    /// tier's `hooked` and the host's `extended_info` ([`build_identity`]).
    pub(crate) fn new(
        capacity: usize,
        hooked: php_compile::Hooked,
        extended_info: bool,
        functions: &[php_unit::decl::FunctionDecl],
        classes: &[php_unit::decl::ClassDecl],
    ) -> UnitCache {
        let names = functions.iter().map(|decl| &*decl.name.0);
        let build = build_identity(
            hooked,
            extended_info,
            names.chain(classes.iter().map(|decl| &*decl.name.0)),
        );
        UnitCache {
            held: Cache::new(capacity),
            taken: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            blacklist_misses: AtomicU64::new(0),
            start_time: now(),
            last_restart_time: AtomicI64::new(0),
            manual_restarts: AtomicU64::new(0),
            oom_restarts: AtomicU64::new(0),
            hash_restarts: AtomicU64::new(0),
            restart_pending: AtomicBool::new(false),
            restart_reason: AtomicU8::new(Reason::User as u8),
            used: AtomicU64::new(0),
            wasted: AtomicU64::new(0),
            exhausted: AtomicBool::new(false),
            budget: OnceLock::new(),
            preloaded: OnceLock::new(),
            build,
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

    /// The file held under `key`, a full path or a path as written: a
    /// saved file of the preload first, as php's hash holds them.
    pub(crate) fn find(&self, key: &UnitKey) -> Option<Arc<Held>> {
        if let Some(held) = self
            .preloaded()
            .and_then(|preloaded| preloaded.script(&key.0))
        {
            return Some(Arc::clone(held));
        }
        self.held.get(key)
    }

    /// `zend_accel_add_key`: `key` filed beside the names `held` is held
    /// under.
    pub(crate) fn add_key(&self, key: UnitKey, held: &Arc<Held>) {
        if self
            .held
            .get(&key)
            .is_some_and(|found| Arc::ptr_eq(&found, held))
        {
            return;
        }
        if self.hash_full() {
            return;
        }
        self.held.alias(key, held);
    }

    /// The bounds the extension set, once: the first stands.
    pub(crate) fn set_budget(&self, budget: CacheBudget) {
        self.budget.set(budget).ok();
    }

    /// `zend_accel_hash_is_full`, which marks the memory exhausted and
    /// schedules a restart where enough is wasted.
    fn hash_full(&self) -> bool {
        let Some(budget) = self.budget.get() else {
            return false;
        };
        if self.held.len() < usize::try_from(budget.keys).unwrap_or(usize::MAX) {
            return false;
        }
        self.exhausted.store(true, Ordering::Relaxed);
        self.schedule_restart_if_necessary(Reason::Hash);
        true
    }

    /// Whether a file weighing `size` finds room: a name free in the
    /// hash, and `size` within what is neither used nor wasted, as
    /// `zend_shared_alloc_aligned` finds it; else the memory exhausted
    /// and a restart scheduled where enough is wasted, and the file is
    /// held nowhere.
    fn room_for(&self, size: u64) -> bool {
        if self.hash_full() {
            return false;
        }
        let Some(budget) = self.budget.get() else {
            return true;
        };
        let preload = self.preloaded().map_or(0, |preloaded| {
            let scripts = preloaded.scripts.iter().map(|held| held.size);
            scripts.fold(preloaded.statistics.memory_consumption, u64::saturating_add)
        });
        let taken = (self.used.load(Ordering::Relaxed))
            .saturating_add(self.wasted.load(Ordering::Relaxed))
            .saturating_add(preload);
        if budget.memory.saturating_sub(taken) >= size {
            return true;
        }
        self.exhausted.store(true, Ordering::Relaxed);
        self.schedule_restart_if_necessary(Reason::Oom);
        false
    }

    /// `zend_accel_discard_script`: `held` dropped under every name it is
    /// filed under, its weight counted as wasted once however many
    /// requests discard it, and where the memory is exhausted a restart
    /// scheduled if enough is now wasted.
    fn discard_held(&self, held: &Arc<Held>) {
        self.held.remove_all(held);
        if held.discarded.swap(true, Ordering::Relaxed) {
            return;
        }
        self.used.fetch_sub(
            held.size.min(self.used.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
        self.wasted.fetch_add(held.size, Ordering::Relaxed);
        if self.exhausted.load(Ordering::Relaxed) {
            let reason = if self.hash_full() {
                Reason::Hash
            } else {
                Reason::Oom
            };
            self.schedule_restart_if_necessary(reason);
        }
    }

    /// A hit on `held`, counted on the cache and on the file, which was
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

    /// `persistent_compile_file` by the file's full path: the file held
    /// under `path` where `validation` finds it fresh at the request time
    /// `now`, a hit; else the stale copy discarded under every name it
    /// was filed under, the file cache asked where there is one, a hit
    /// where it keeps the file as it stands, `decode`d and held under the
    /// full path alone; else a
    /// miss, the file read and `compile`d, and what it compiled to held,
    /// or what another request held for the path meanwhile, filed under
    /// `written` too where given, and kept in the file cache. A file
    /// whose stamp the validation or the `protection` needs and the
    /// system does not give, and one written within `protection` seconds
    /// of `now`, is compiled and held and kept nowhere, as php behaves
    /// without the cache; any miss while a restart is pending is kept in
    /// the file cache alone. [`Levels::Disk`] asks and keeps in the file
    /// cache alone and counts nothing. Nothing where the file cannot be
    /// read.
    pub(crate) fn file(
        &self,
        asked: Asked<'_>,
        compile: impl FnOnce(&[u8]) -> Result<Arc<Unit>, CompileError>,
        decode: impl FnOnce(&[u8]) -> Option<Arc<Unit>>,
        publish: impl FnOnce(&Arc<Unit>) -> EarlyBound,
    ) -> (Cached, Filed, Option<EarlyBound>) {
        let Asked {
            path,
            written,
            validation,
            protection,
            now,
            levels,
        } = asked;
        let (memory, disk) = match levels {
            Levels::Memory => (true, None),
            Levels::Both(disk) => (true, Some(disk)),
            Levels::Disk(disk) => (false, Some(disk)),
        };
        if memory && let Some(hit) = self.held_hit(path, written.as_ref(), validation, now) {
            return (
                Some(hit.unit.clone()),
                Filed::Hit,
                Some(Arc::clone(&hit.early)),
            );
        }
        let validated = validation != Validation::Never;
        let stamped = validated || protection > 0;
        let observed = if stamped { Stamp::of(path) } else { None };
        // The second level, `zend_file_cache_script_load`: read back
        // where it keeps the file at the `mtime` it has now, where
        // timestamps are validated.
        if let Some(disk) = &disk
            && (!validated || observed.is_some())
            && let Some(kept) = disk.load(path, observed.filter(|_| validated).map(Stamp::seconds))
            && let Some(unit) = decode(&kept.body)
        {
            // Held as `zend_file_cache_script_load` puts it in the shared
            // hash, a hit, except while a restart is pending.
            if !memory {
                return (Some(Ok(unit)), Filed::Unheld, None);
            }
            if self.restart_pending.load(Ordering::Relaxed) || !self.room_for(kept.size) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return (Some(Ok(unit)), Filed::Unheld, None);
            }
            // Under its full path alone: `zend_file_cache_script_load`
            // files no key, which the next include adds after opening it.
            let early = publish(&unit);
            let read = Read {
                unit: Ok(unit),
                early,
                size: kept.size,
                observed,
            };
            let held = self.hold(path, validation, now, read);
            self.hit(&held, validation, now);
            return (
                Some(held.unit.clone()),
                Filed::Hit,
                Some(Arc::clone(&held.early)),
            );
        }
        if memory && validation != Validation::EveryHit {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        let Ok(source) = std::fs::read(String::from_utf8_lossy(path).as_ref()) else {
            return (None, Filed::Unheld, None);
        };
        let unit = compile(&source);
        let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
        let unheld = stamped
            && observed.is_none_or(|stamp| {
                protection > 0 && now.saturating_sub(i64::from(protection)) < stamp.seconds()
            });
        if unheld {
            return (Some(unit), Filed::Compiled, None);
        }
        if let (Some(disk), Ok(compiled)) = (&disk, &unit)
            && let Some(body) = wire::encode(compiled)
        {
            let stamp = observed.filter(|_| validated).map_or(0, Stamp::seconds);
            disk.store(path, stamp, size, &body);
        }
        if !memory || self.restart_pending.load(Ordering::Relaxed) || !self.room_for(size) {
            return (
                Some(unit),
                disk.map_or(Filed::Compiled, |_| Filed::Unheld),
                None,
            );
        }
        let early = match &unit {
            Ok(compiled) => publish(compiled),
            Err(_) => Arc::new([]),
        };
        let read = Read {
            unit,
            early,
            size,
            observed,
        };
        let held = self.hold(path, validation, now, read);
        if let Some(written) = written {
            self.add_key(written, &held);
        }
        (
            Some(held.unit.clone()),
            Filed::Held,
            Some(Arc::clone(&held.early)),
        )
    }

    /// The first level's hit on `path`: a file the preload saved,
    /// whose timestamp php keeps at zero, served as held without the
    /// file looked at, whatever the window, and a held file `validation`
    /// finds fresh at `now`; each filed under `written` too. A stale copy
    /// is discarded under every name it was filed under.
    fn held_hit(
        &self,
        path: &[u8],
        written: Option<&UnitKey>,
        validation: Validation,
        now: i64,
    ) -> Option<Arc<Held>> {
        // The cache bypassed, `EveryHit`, compiles afresh.
        if validation != Validation::EveryHit
            && let Some(held) = self
                .preloaded()
                .and_then(|preloaded| preloaded.script(path))
        {
            self.hit(held, validation, now);
            if let Some(written) = written {
                self.add_key(written.clone(), held);
            }
            return Some(Arc::clone(held));
        }
        let stale = self.held.get(&UnitKey::full(path))?;
        if stale.validated(validation, now) {
            self.hit(&stale, validation, now);
            if let Some(written) = written {
                self.add_key(written.clone(), &stale);
            }
            return Some(stale);
        }
        self.discard_held(&stale);
        None
    }

    /// What `read` compiled from the file at `path`, held under its full
    /// path; what another request held for it meanwhile where it did.
    fn hold(&self, path: &[u8], validation: Validation, now: i64, read: Read) -> Arc<Held> {
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
        let order = self.taken.fetch_add(1, Ordering::Relaxed);
        let held = self.held.insert(
            UnitKey::full(path),
            Held {
                unit,
                early,
                path: path.to_vec(),
                stamp,
                size,
                order,
                hits: AtomicU64::new(0),
                last_used: AtomicI64::new(now),
                revalidate: AtomicI64::new(revalidate),
                discarded: AtomicBool::new(false),
            },
        );
        if held.order == order {
            self.used.fetch_add(size, Ordering::Relaxed);
        }
        held
    }

    /// `ZCSG(blacklist_misses)++`: a file the blacklist kept out of the
    /// cache was compiled for its request.
    pub(crate) fn note_blacklisted(&self) {
        self.blacklist_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// `zend_accel_invalidate`'s discard of the file held under `key`,
    /// with every name it is filed under: where `force`, and otherwise
    /// where it changed since, `do_validate_timestamps` finding it filed
    /// under another full path, held with no stamp, or written at
    /// another second. Whether a unit was held under the name at all; a
    /// refusal is none, as php persists none.
    pub(crate) fn discard(&self, key: &UnitKey, force: bool) -> bool {
        let Some(held) = self.held.get(key) else {
            return false;
        };
        if held.unit.is_err() {
            return false;
        }
        let changed = force
            || key.0 != held.path
            || held.stamp.is_none_or(|was| {
                Stamp::of(&held.path).is_none_or(|stamp| stamp.seconds() != was.seconds())
            });
        if changed {
            self.discard_held(&held);
        }
        true
    }

    /// `zend_accel_schedule_restart(ACCEL_RESTART_USER)`: the cache is
    /// emptied at the start of the first request no other request runs
    /// beside, and until then nothing new is held; a second call while
    /// one is pending schedules nothing.
    pub(crate) fn schedule_restart(&self) -> Restart {
        self.schedule(Reason::User)
    }

    /// `zend_accel_schedule_restart` for `reason`, which a restart
    /// already pending keeps its own reason over.
    fn schedule(&self, reason: Reason) -> Restart {
        if self.restart_pending.swap(true, Ordering::Relaxed) {
            return Restart::Pending;
        }
        self.restart_reason.store(reason as u8, Ordering::Relaxed);
        Restart::Scheduled
    }

    /// `zend_accel_schedule_restart_if_necessary`: a restart for `reason`
    /// where what is wasted reaches `opcache.max_wasted_percentage` of
    /// `opcache.memory_consumption`.
    fn schedule_restart_if_necessary(&self, reason: Reason) {
        let Some(budget) = self.budget.get() else {
            return;
        };
        let wasted = self.wasted.load(Ordering::Relaxed);
        let reached = u128::from(wasted).saturating_mul(100)
            >= u128::from(budget.memory).saturating_mul(u128::from(budget.max_wasted));
        if reached {
            self.schedule(reason);
        }
    }

    /// `ZCSG(restart_pending)`.
    pub(crate) fn restart_pending(&self) -> bool {
        self.restart_pending.load(Ordering::Relaxed)
    }

    /// `accel_activate`'s restart, made where one is pending and no
    /// request runs ([`crate::arena::Epochs::enter`]): the restart counted
    /// under its reason, the cache emptied, nothing wasted or exhausted
    /// any more (`zend_shared_alloc_restore_state`), and the time stamped,
    /// one second past the last where two fall in one second. The
    /// preload's scripts stay, as php keeps them.
    pub(crate) fn restart(&self) {
        if !self.restart_pending.swap(false, Ordering::Relaxed) {
            return;
        }
        let counted = match Reason::of(self.restart_reason.load(Ordering::Relaxed)) {
            Reason::Oom => &self.oom_restarts,
            Reason::Hash => &self.hash_restarts,
            Reason::User => &self.manual_restarts,
        };
        counted.fetch_add(1, Ordering::Relaxed);
        self.held.retain(|_, _| false);
        self.used.store(0, Ordering::Relaxed);
        self.wasted.store(0, Ordering::Relaxed);
        self.exhausted.store(false, Ordering::Relaxed);
        let now = now();
        let last = self.last_restart_time.load(Ordering::Relaxed);
        let stamped = if last < now {
            now
        } else {
            last.saturating_add(1)
        };
        self.last_restart_time.store(stamped, Ordering::Relaxed);
    }

    /// What `opcache_get_status()` reads: the counters, the names held,
    /// and every file held under its full path, in the order the cache
    /// took them; a refusal is none of either.
    pub(crate) fn status(&self, interned_strings: usize) -> CacheStatus {
        let mut held = self.held.held();
        held.retain(|(_, held)| held.unit.is_ok());
        held.retain(|(_, held)| {
            self.preloaded()
                .is_none_or(|preloaded| preloaded.script(&held.path).is_none())
        });
        let mut keys = u64::try_from(held.len()).unwrap_or(u64::MAX);
        held.retain(|(key, held)| key.0 == held.path);
        held.sort_by_key(|(_, held)| held.order);
        let listed = |held: &Held| CachedFile {
            path: held.path.clone(),
            hits: held.hits.load(Ordering::Relaxed),
            size: held.size,
            last_used: held.last_used.load(Ordering::Relaxed),
            timestamp: held.stamp.map_or(0, Stamp::seconds),
            revalidate: held.revalidate.load(Ordering::Relaxed),
        };
        // The preload's pseudo-file and its saved scripts stand in the
        // hash beside what the requests cached, never validated.
        let mut files = Vec::new();
        if let Some(preloaded) = self.preloaded() {
            files.push(CachedFile {
                path: PRELOAD_SCRIPT.to_vec(),
                hits: 0,
                size: preloaded.statistics.memory_consumption,
                last_used: 0,
                timestamp: 0,
                revalidate: 0,
            });
            files.extend(preloaded.scripts.iter().map(|held| listed(held)));
            let preloaded = u64::try_from(files.len()).unwrap_or(u64::MAX);
            keys = keys.saturating_add(preloaded);
        }
        files.extend(held.into_iter().map(|(_, held)| listed(&held)));
        CacheStatus {
            restart_pending: self.restart_pending.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            blacklist_misses: self.blacklist_misses.load(Ordering::Relaxed),
            start_time: self.start_time,
            last_restart_time: self.last_restart_time.load(Ordering::Relaxed),
            manual_restarts: self.manual_restarts.load(Ordering::Relaxed),
            oom_restarts: self.oom_restarts.load(Ordering::Relaxed),
            hash_restarts: self.hash_restarts.load(Ordering::Relaxed),
            wasted: self.wasted.load(Ordering::Relaxed),
            cache_full: self.exhausted.load(Ordering::Relaxed),
            interned_strings: u64::try_from(interned_strings).unwrap_or(u64::MAX),
            keys,
            files,
            preload: self
                .preloaded()
                .map(|preloaded| preloaded.statistics.clone()),
        }
    }
}

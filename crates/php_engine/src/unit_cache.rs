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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use php_compile::CompileError;
use php_unit::Unit;

use crate::api::abi::{Cache, Validation};
use crate::api::request::{CacheStatus, CachedFile, Preloaded, Restart};

use disk::build_identity;
pub(crate) use disk::{BUILD, Disk};
pub(crate) use wire::decode;

/// What the unit cache answers for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read.
pub(crate) type Cached = Option<Result<Arc<Unit>, CompileError>>;

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
            path,
            stamp: None,
            size,
            order,
            hits: AtomicU64::new(0),
            last_used: AtomicI64::new(0),
            revalidate: AtomicI64::new(0),
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
    restart_pending: AtomicBool,
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
    /// Compiled and held nowhere.
    Unheld,
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
    /// over a bundle that declares `functions` and `classes`
    /// ([`build_identity`]).
    pub(crate) fn new(
        capacity: usize,
        functions: &[php_unit::decl::FunctionDecl],
        classes: &[php_unit::decl::ClassDecl],
    ) -> UnitCache {
        let names = functions.iter().map(|decl| &*decl.name.0);
        let build = build_identity(names.chain(classes.iter().map(|decl| &*decl.name.0)));
        UnitCache {
            held: Cache::new(capacity),
            taken: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            blacklist_misses: AtomicU64::new(0),
            start_time: now(),
            last_restart_time: AtomicI64::new(0),
            manual_restarts: AtomicU64::new(0),
            restart_pending: AtomicBool::new(false),
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
        let _ = self.preloaded.set(preloaded);
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
    /// where it keeps the file as it stands, `decode`d and held; else a
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
    ) -> (Cached, Filed) {
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
            return hit;
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
                return (Some(Ok(unit)), Filed::Unheld);
            }
            if self.restart_pending.load(Ordering::Relaxed) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return (Some(Ok(unit)), Filed::Unheld);
            }
            let held = self.hold(path, validation, now, Ok(unit), kept.size, observed);
            self.hit(&held, validation, now);
            if let Some(written) = written {
                self.held.alias(written, &held);
            }
            return (Some(held.unit.clone()), Filed::Hit);
        }
        if memory && validation != Validation::EveryHit {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        let Ok(source) = std::fs::read(String::from_utf8_lossy(path).as_ref()) else {
            return (None, Filed::Unheld);
        };
        let unit = compile(&source);
        let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
        let unheld = stamped
            && observed.is_none_or(|stamp| {
                protection > 0 && now.saturating_sub(i64::from(protection)) < stamp.seconds()
            });
        if unheld {
            return (Some(unit), Filed::Unheld);
        }
        if let (Some(disk), Ok(compiled)) = (&disk, &unit)
            && let Some(body) = wire::encode(compiled)
        {
            let stamp = observed.filter(|_| validated).map_or(0, Stamp::seconds);
            disk.store(path, stamp, size, &body);
        }
        if !memory || self.restart_pending.load(Ordering::Relaxed) {
            return (Some(unit), Filed::Unheld);
        }
        let held = self.hold(path, validation, now, unit, size, observed);
        if let Some(written) = written {
            self.held.alias(written, &held);
        }
        (Some(held.unit.clone()), Filed::Held)
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
    ) -> Option<(Cached, Filed)> {
        // The cache bypassed, `EveryHit`, compiles afresh.
        if validation != Validation::EveryHit
            && let Some(held) = self
                .preloaded()
                .and_then(|preloaded| preloaded.script(path))
        {
            self.hit(held, validation, now);
            if let Some(written) = written {
                self.held.alias(written.clone(), held);
            }
            return Some((Some(held.unit.clone()), Filed::Hit));
        }
        let stale = self.held.get(&UnitKey::full(path))?;
        if stale.validated(validation, now) {
            self.hit(&stale, validation, now);
            if let Some(written) = written {
                self.held.alias(written.clone(), &stale);
            }
            return Some((Some(stale.unit.clone()), Filed::Hit));
        }
        self.held.remove_all(&stale);
        None
    }

    /// `unit`, compiled from `size` bytes of the file at `path` whose
    /// stamp was `observed`, held under its full path; what another
    /// request held for it meanwhile where it did.
    fn hold(
        &self,
        path: &[u8],
        validation: Validation,
        now: i64,
        unit: Result<Arc<Unit>, CompileError>,
        size: u64,
        observed: Option<Stamp>,
    ) -> Arc<Held> {
        let stamp = match validation {
            Validation::Never => None,
            Validation::Window { .. } | Validation::EveryHit => observed,
        };
        let revalidate = match validation {
            Validation::Window { seconds } => now.saturating_add(i64::from(seconds)),
            Validation::Never | Validation::EveryHit => 0,
        };
        self.held.insert(
            UnitKey::full(path),
            Held {
                unit,
                path: path.to_vec(),
                stamp,
                size,
                order: self.taken.fetch_add(1, Ordering::Relaxed),
                hits: AtomicU64::new(0),
                last_used: AtomicI64::new(now),
                revalidate: AtomicI64::new(revalidate),
            },
        )
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
            self.held.remove_all(&held);
        }
        true
    }

    /// `zend_accel_schedule_restart(ACCEL_RESTART_USER)`: the cache is
    /// emptied at the next request's start, and until then nothing new
    /// is held; a second call while one is pending schedules nothing.
    pub(crate) fn schedule_restart(&self) -> Restart {
        if self.restart_pending.swap(true, Ordering::Relaxed) {
            Restart::Pending
        } else {
            Restart::Scheduled
        }
    }

    /// `accel_activate`'s restart at a request's start: a pending restart
    /// empties the cache, counts a manual restart and stamps the time,
    /// one second past the last where two fall in one second.
    pub(crate) fn restart_if_pending(&self) {
        if !self.restart_pending.swap(false, Ordering::Relaxed) {
            return;
        }
        self.manual_restarts.fetch_add(1, Ordering::Relaxed);
        self.held.retain(|_, _| false);
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
        let mut held = self.held.snapshot();
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
            interned_strings: u64::try_from(interned_strings).unwrap_or(u64::MAX),
            keys,
            files,
            preload: self
                .preloaded()
                .map(|preloaded| preloaded.statistics.clone()),
        }
    }
}

//! The unit cache (design §6.3): what a file compiled to, held for every
//! request that asks for the same file unchanged, over the engine's one
//! [`Cache`] (ADR-0005), with the counters opcache keeps beside its
//! shared hash. php's `zend_persistent_script` is [`Held`], its
//! `dynamic_members` the hits and the last use on it; `ZCSG(hits)`,
//! `ZCSG(misses)`, `ZCSG(blacklist_misses)`, the restart counters and
//! `ZCSG(restart_pending)` are [`UnitCache`]'s. What reads them is the
//! request's face, [`crate::api::request::UnitCache`], which `opcache_*`
//! answer from.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use php_compile::CompileError;
use php_unit::Unit;

use crate::api::abi::Cache;
use crate::api::request::{CacheStatus, CachedFile, Restart};

/// What the unit cache holds for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read when it was asked for.
pub(crate) type Cached = Option<Result<Arc<Unit>, CompileError>>;

/// What the unit cache is keyed by (design §6.3): the file's name and
/// what says it has not changed under the engine, php's realpath with
/// its `(mtime, size)`. A file edited in place is a key of its own, so
/// what was cached for the old bytes is never served for the new.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct UnitKey {
    path: Vec<u8>,
    /// What the system reports as the file's last write, php's
    /// `zend_stat_t.st_mtime`.
    modified: Option<Duration>,
    size: u64,
}

impl UnitKey {
    /// The key of the file at `path` as the system reports it now, the
    /// path made whole against `cwd` where it is relative, as php keys a
    /// file by its full path; none where nothing is there.
    pub(crate) fn of(path: &[u8], cwd: &Path) -> Option<UnitKey> {
        let name = String::from_utf8_lossy(path).into_owned();
        let described = std::fs::metadata(&name).ok()?;
        let path = if path.starts_with(b"/") {
            path.to_vec()
        } else {
            std::fs::canonicalize(cwd.join(&name)).map_or_else(
                |_| path.to_vec(),
                |full| full.into_os_string().into_encoded_bytes(),
            )
        };
        Some(UnitKey {
            path,
            modified: described
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok()),
            size: described.len(),
        })
    }

    /// The file's full path, what `opcache_get_status()` keys a file by.
    pub(crate) fn path(&self) -> &[u8] {
        &self.path
    }
}

/// One file the cache holds, php's `zend_persistent_script`: what it
/// compiled to and its `dynamic_members`, the hits since it was cached
/// and when it was last served.
pub(crate) struct Held {
    pub(crate) unit: Cached,
    /// The order the cache took it in, which its status lists it by.
    order: u64,
    hits: AtomicU64,
    last_used: AtomicI64,
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
    /// An empty cache holding at most `capacity` files.
    pub(crate) fn new(capacity: usize) -> UnitCache {
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
        }
    }

    /// `persistent_compile_file` over the cache: the file under `key` as
    /// held, a hit counted on the cache and on the file; else a miss,
    /// `build` compiling it, and what it compiled to held in place of
    /// what the cache had for an older copy of the same path, which
    /// `validate_timestamps` discards. While a restart is pending a miss
    /// is compiled and held nowhere, as php behaves without the cache.
    pub(crate) fn file(&self, key: UnitKey, build: impl FnOnce() -> Cached) -> Cached {
        if let Some(found) = self.held.get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            found.hits.fetch_add(1, Ordering::Relaxed);
            found.last_used.store(now(), Ordering::Relaxed);
            return found.unit.clone();
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let unit = build();
        if self.restart_pending.load(Ordering::Relaxed) {
            return unit;
        }
        self.held
            .retain(|held, _| held.path != key.path || *held == key);
        let held = self.held.insert(
            key,
            Held {
                unit,
                order: self.taken.fetch_add(1, Ordering::Relaxed),
                hits: AtomicU64::new(0),
                last_used: AtomicI64::new(now()),
            },
        );
        held.unit.clone()
    }

    /// `ZCSG(blacklist_misses)++`: a file the blacklist kept out of the
    /// cache was compiled for its request.
    pub(crate) fn note_blacklisted(&self) {
        self.blacklist_misses.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the cache holds a unit for the file as `key` has it,
    /// `filename_is_in_cache` with its timestamps validated: a refusal
    /// is no unit, as php persists none.
    pub(crate) fn holds(&self, key: &UnitKey) -> bool {
        self.held
            .get(key)
            .is_some_and(|held| matches!(held.unit, Some(Ok(_))))
    }

    /// `zend_accel_invalidate`'s discard over what the cache holds for
    /// `path`: every copy where `force`, and otherwise the copy whose
    /// file changed since, `current` being the file as it stands. Whether
    /// the cache held a unit for the path at all.
    pub(crate) fn discard(&self, path: &[u8], current: Option<&UnitKey>, force: bool) -> bool {
        let mut found = false;
        self.held.retain(|key, held| {
            if key.path != path || !matches!(held.unit, Some(Ok(_))) {
                return true;
            }
            found = true;
            !force && current == Some(key)
        });
        found
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

    /// What `opcache_get_status()` reads: the counters and every file
    /// held, in the order the cache took them.
    pub(crate) fn status(&self, interned_strings: usize) -> CacheStatus {
        let mut held = self.held.snapshot();
        held.sort_by_key(|(_, held)| held.order);
        let files = held
            .into_iter()
            .filter(|(_, held)| matches!(held.unit, Some(Ok(_))))
            .map(|(key, held)| CachedFile {
                path: key.path,
                hits: held.hits.load(Ordering::Relaxed),
                size: key.size,
                last_used: held.last_used.load(Ordering::Relaxed),
                timestamp: key
                    .modified
                    .and_then(|at| i64::try_from(at.as_secs()).ok())
                    .unwrap_or(0),
            })
            .collect();
        CacheStatus {
            restart_pending: self.restart_pending.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            blacklist_misses: self.blacklist_misses.load(Ordering::Relaxed),
            start_time: self.start_time,
            last_restart_time: self.last_restart_time.load(Ordering::Relaxed),
            manual_restarts: self.manual_restarts.load(Ordering::Relaxed),
            interned_strings: u64::try_from(interned_strings).unwrap_or(u64::MAX),
            files,
        }
    }
}

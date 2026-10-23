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
    pub(crate) unit: Result<PublishedUnit, CompileError>,
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
    /// `corrupted`: discarded, its weight counted as wasted once. It
    /// stays filed and listed, a miss to every lookup, until a compile
    /// of its path replaces it or the epoch goes.
    discarded: AtomicBool,
}

/// What a file read from disk came to, for [`UnitCache::hold`]: the
/// unit or the compiler's refusal, its early-bound classes published,
/// the bytes it was read from and its stamp as observed.
struct Read {
    unit: Result<PublishedUnit, CompileError>,
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
    pub(crate) fn preloaded(unit: PublishedUnit, path: Vec<u8>, size: u64, order: u64) -> Held {
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
    held: Cache<UnitKey, Held>,
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
/// what it compiles.
pub(crate) struct Asked<'a> {
    pub(crate) path: &'a [u8],
    pub(crate) source: Option<&'a [u8]>,
    pub(crate) written: Option<UnitKey>,
    pub(crate) validation: Validation,
    pub(crate) protection: u32,
    pub(crate) now: i64,
    pub(crate) levels: Levels<'a>,
    pub(crate) optimizer: Optimizer,
    /// Whether the safepoint words of a unit held for this question count
    /// (`tier::Tier::counts`), as the tier answered for the request
    /// asking.
    pub(crate) counts: bool,
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
/// heap ([`php_unit::OpArray::cache_room`], [`compact`]). So where the
/// accelerator holds a file, sites naming one function or one class
/// share one slot, and so do the sites naming one constant, and one
/// property or one method of `$this` where the op array is sure of it, a
/// method's or a closure's written in one; a closure's own cache,
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
    /// No pass: `opcache.optimization_level` leaves pass 11 out.
    Off,
    /// Pass 11's count, from the first slot past the op array
    /// extensions' (`zend_op_array_extension_handles * sizeof(void*)`).
    CompactLiterals { first: u32 },
}

impl Optimizer {
    /// `ZEND_OPTIMIZER_PASS_11` (`Z/Zend/Optimizer/zend_optimizer.h:38`).
    pub(crate) const PASS_11: i64 = 1 << 10;

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
        if let Optimizer::CompactLiterals { first } = self {
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
    pub(crate) fn find(&self, files: &Files, key: &UnitKey) -> Option<Arc<Held>> {
        if let Some(held) = self
            .preloaded()
            .and_then(|preloaded| preloaded.script(&key.0))
        {
            return Some(Arc::clone(held));
        }
        files.held.get(key).filter(|held| !held.discarded())
    }

    /// `zend_accel_add_key`: `key` filed beside the names `held` is held
    /// under in `files`.
    pub(crate) fn add_key(&self, files: &Files, key: UnitKey, held: &Arc<Held>) {
        if files
            .held
            .get(&key)
            .is_some_and(|found| Arc::ptr_eq(&found, held))
        {
            return;
        }
        if self.hash_full(files) {
            return;
        }
        // `accel_new_interned_key`: the name as written is one of the
        // buffer's strings from here, a unit's alone, as php persists
        // no refusal.
        if held.unit.is_ok() {
            php_heap::Intern::intern(&mut *self.interned.strings(), &key.0).ok();
        }
        files.held.alias(key, held);
    }

    /// The names `zend_accel_script_persist` interns beside the strings
    /// of a unit it persists, once the unit is held under `path`: its
    /// `filename`, the full path, and the keys of its `function_table`
    /// and `class_table` ([`table_keys`]). A unit served without being
    /// held is persisted by nothing, and names none.
    pub(crate) fn persist_names(&self, path: &[u8], unit: &Unit) {
        let mut strings = self.interned.strings();
        php_heap::Intern::intern(&mut *strings, path).ok();
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
    fn discard_held(&self, files: &Files, held: &Held) {
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
    /// system does not give, and one written within `protection` seconds
    /// of `now`, is compiled and held and kept nowhere, as php behaves
    /// without the cache, and so is a unit [`named_by_request`]; any miss
    /// while a restart is pending is kept in
    /// the file cache alone. [`Levels::Disk`] asks and keeps in the file
    /// cache alone and counts nothing. Nothing where the file cannot be
    /// read. What is `compile`d or `decode`d takes no map-pointer offset
    /// until it is held, `share`d as it is; one held nowhere is answered
    /// [`Compiled::Alone`], its caller's own (GH-8646), and so is one
    /// that answers the request that asked alone ([`Answers::Asker`]).
    /// `compile` and `publish` are handed `caller`, the request the file
    /// is compiled and its classes published for, or nothing.
    #[allow(clippy::too_many_arguments)] // the table, the question, the caller and the four steps, each one fact
    pub(crate) fn file<A>(
        &self,
        caller: &mut A,
        files: &Files,
        asked: Asked<'_>,
        compile: impl FnOnce(&mut A, &[u8]) -> Fresh,
        decode: impl FnOnce(&[u8]) -> Option<Unit>,
        share: impl FnOnce(Unit) -> PublishedUnit,
        publish: impl FnOnce(&mut A, &PublishedUnit) -> EarlyBound,
    ) -> Answered {
        let Asked {
            path,
            source: handed,
            written,
            validation,
            protection,
            now,
            levels,
            optimizer,
            counts,
        } = asked;
        let (memory, disk) = match levels {
            Levels::Memory => (true, None),
            Levels::Both(disk) => (true, Some(disk)),
            Levels::Disk(disk) => (false, Some(disk)),
        };
        if memory && let Some(hit) = self.held_hit(files, path, written.as_ref(), validation, now) {
            return (
                Some(hit.unit.clone().map(Compiled::Published)),
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
                return alone(Ok(unit), Filed::Unheld);
            }
            if files.restart_pending() || !self.room_for(files, kept.size) {
                files.hits.fetch_add(1, Ordering::Relaxed);
                return alone(Ok(unit), Filed::Unheld);
            }
            // Under its full path alone: `zend_file_cache_script_load`
            // files no key, which the next include adds after opening it.
            let unit = share(unit);
            let early = publish(caller, &unit);
            let read = Read {
                unit: Ok(unit),
                early,
                size: kept.size,
                observed,
            };
            let held = self.hold(files, (path, counts), validation, now, read);
            files.hit(&held, validation, now);
            return (
                Some(held.unit.clone().map(Compiled::Published)),
                Filed::Hit,
                Some(Arc::clone(&held.early)),
            );
        }
        if memory && validation != Validation::EveryHit {
            files.misses.fetch_add(1, Ordering::Relaxed);
        }
        let Some(source) = source_or_read(handed, path) else {
            return (None, Filed::Unheld, None);
        };
        let Fresh { unit, answers } = compile(caller, &source);
        let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
        let unheld = stamped
            && observed.is_none_or(|stamp| {
                protection > 0 && now.saturating_sub(i64::from(protection)) < stamp.seconds()
            });
        if unheld || answers == Answers::Asker || named_by_request(validation, &unit) {
            return alone(unit, Filed::Compiled);
        }
        let optimizer = optimizer.on_miss(validation, disk.is_some() || !files.restart_pending());
        let unit = unit.map(|unit| optimizer.optimized(unit));
        if let (Some(disk), Ok(compiled)) = (&disk, &unit)
            && let Some(body) = wire::encode(compiled)
        {
            let stamp = observed.filter(|_| validated).map_or(0, Stamp::seconds);
            disk.store(path, stamp, size, &body);
        }
        if !memory || files.restart_pending() || !self.room_for(files, size) {
            return alone(unit, disk.map_or(Filed::Compiled, |_| Filed::Kept));
        }
        let unit = unit.map(share);
        let early = match &unit {
            Ok(compiled) => publish(caller, compiled),
            Err(_) => Arc::new([]),
        };
        let read = Read {
            unit,
            early,
            size,
            observed,
        };
        let held = self.hold(files, (path, counts), validation, now, read);
        if let Some(written) = written {
            self.add_key(files, written, &held);
        }
        (
            Some(held.unit.clone().map(Compiled::Published)),
            Filed::Held,
            Some(Arc::clone(&held.early)),
        )
    }

    /// The first level's hit on `path`: a file the preload saved,
    /// whose timestamp php keeps at zero, served as held without the
    /// file looked at, whatever the window, and a held file `validation`
    /// finds fresh at `now`; each filed under `written` too. A stale copy
    /// is discarded, and a discarded one is a miss.
    fn held_hit(
        &self,
        files: &Files,
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
            files.hit(held, validation, now);
            if let Some(written) = written {
                self.add_key(files, written.clone(), held);
            }
            return Some(Arc::clone(held));
        }
        let stale = files
            .held
            .get(&UnitKey::full(path))
            .filter(|held| !held.discarded())?;
        if stale.validated(validation, now) {
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
    /// updates the bucket every key of it points to; what another request
    /// held for it meanwhile where it did; its op arrays' words
    /// `counts` or sealed as the tier answered (`tier::Tier::counts`).
    fn hold(
        &self,
        files: &Files,
        (path, counts): (&[u8], bool),
        validation: Validation,
        now: i64,
        read: Read,
    ) -> Arc<Held> {
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
        let held = files.held.insert_over(
            UnitKey::full(path),
            Held::discarded,
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
            files.used.fetch_add(size, Ordering::Relaxed);
            if let Ok(unit) = &held.unit {
                files.funcs.publish(unit, counts);
                self.persist_names(path, unit);
            }
        }
        held
    }

    /// `zend_accel_invalidate`'s discard of the file held under `key`:
    /// where `force`, and otherwise where it changed since,
    /// `do_validate_timestamps` finding it filed under another full path,
    /// held with no stamp, or written at another second. Whether a unit
    /// was held under the name at all; a refusal is none, as php
    /// persists none, and so is a copy discarded already.
    pub(crate) fn discard(&self, files: &Files, key: &UnitKey, force: bool) -> bool {
        let Some(held) = files.held.get(key).filter(|held| !held.discarded()) else {
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
    /// full path, in the order the cache took them; a refusal is none of
    /// either.
    pub(crate) fn status(
        &self,
        files: &Files,
        interned_strings: usize,
        tier: TierStatus,
    ) -> CacheStatus {
        let mut held = files.held.held();
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
            files: listed_files,
            preload: self
                .preloaded()
                .map(|preloaded| preloaded.statistics.clone()),
            tier,
        }
    }
}

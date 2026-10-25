//! The unit cache as a request reads and moves it (design §6.3): what
//! `opcache_get_status`, `opcache_reset`, `opcache_invalidate`,
//! `opcache_compile_file` and `opcache_is_script_cached` answer from,
//! since 8.5 makes opcache always present. The cache is the engine's and
//! shared by every request of an epoch (`crate::unit_cache`,
//! `crate::arena::Epochs`); an extension reaches it
//! through [`Code::unit_cache`] and nothing else, and decides for
//! itself whether its configuration lets it answer, as opcache's
//! `accelerator_enabled` does.

use php_unit::PublishedUnit;
use std::sync::Arc;

use php_compile::SourceKind;

use super::Code;
use super::include::unreadable;
use super::{AccelServed, Request, Unresolved, is_cacheable_stream_path, is_wrapper_url};
use crate::api::abi::{Admission, FileCache, Lookup, Validation};
use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, PhpString};
use crate::c_number::c_str;
use crate::engine::{CompileHeap, ForRequest};
use crate::exec::IncludeKind;
use crate::supplied::chain::FileArgs;
use crate::supplied::tell;
use crate::unit_cache::{
    Asked, Cached, Disk, Filed, Fresh, Held, Levels, Stamp, Stat, UnitKey, full_path,
};

/// What the `zend_file_handle` a file is compiled through holds when
/// `persistent_compile_file` finds it held, which decides what a due
/// stat costs the request ([`Request::cached_file`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HandlePath {
    /// The path alone, as `compile_filename` hands an `include` or a
    /// `require` over: `do_validate_timestamps` resolves it again.
    Filename,
    /// The opened path too, as `zend_stream_open` leaves it for an
    /// `_once` include or a preload: the stat compares it and resolves
    /// nothing.
    Opened,
    /// The primary file, opened by the host as a `FILE *` and not as a
    /// stream: what [`HandlePath::Opened`] is, with no resource spent.
    Host,
    /// The path alone, as `zend_stream_init_filename` leaves
    /// `auto_prepend_file`'s, `auto_append_file`'s and the built-in
    /// server's router's and program's: what [`HandlePath::Filename`] is,
    /// but destroyed only once the program has been served, by
    /// `php_execute_script_ex` (`Z/main/main.c:2650-2656`) and the server
    /// (`Z/sapi/cli/php_cli_server.c:2138`, `:2282`), so the file its open
    /// made is held open until then ([`Request::require_kept`]).
    Kept,
    /// A stream a wrapper opened, `php_stream_open_for_zend_ex` over a
    /// URL ([`Request::include_stream`]): its bytes compiled under the
    /// path the stream names as opened, held by the cache where opcache's
    /// predicate keeps that path ([`FileArgs::cached`]) and as the
    /// request's own otherwise.
    Stream,
}

impl HandlePath {
    /// Whether the `zend_file_handle` holds the path alone and no opened
    /// path.
    fn path_alone(self) -> bool {
        matches!(self, HandlePath::Filename | HandlePath::Kept)
    }
}

/// The unit cache for one request: [`Code::unit_cache`].
pub struct UnitCache<'r> {
    req: &'r mut Request,
}

/// What `opcache_get_status()` reads of the cache: opcache's shared
/// counters and every file held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStatus {
    /// `ZCSG(restart_pending)`: a reset waits for the next request.
    pub restart_pending: bool,
    /// `ZCSG(hits)`: files served from the cache.
    pub hits: u64,
    /// `ZCSG(misses)`: files compiled for the cache, a blacklisted one
    /// not among them.
    pub misses: u64,
    /// `ZCSG(blacklist_misses)`: files the blacklist kept out.
    pub blacklist_misses: u64,
    /// `ZCSG(start_time)`: when the engine was built, in seconds.
    pub start_time: i64,
    /// `ZCSG(last_restart_time)`: when a reset last emptied the cache,
    /// zero for never.
    pub last_restart_time: i64,
    /// `ZCSG(manual_restarts)`: resets carried out.
    pub manual_restarts: u64,
    /// `ZCSG(oom_restarts)`: restarts the memory running out scheduled.
    pub oom_restarts: u64,
    /// `ZCSG(hash_restarts)`: restarts the names running out scheduled.
    pub hash_restarts: u64,
    /// What the files held weigh, in bytes: the preload's and every file
    /// listed but a discarded one, whose weight is wasted.
    pub used: u64,
    /// `ZSMMG(wasted_shared_memory)`: what the files discarded since the
    /// last restart weighed, in bytes.
    pub wasted: u64,
    /// `ZSMMG(memory_exhausted)`: a file found no room since the last
    /// restart.
    pub cache_full: bool,
    /// The strings the engine's interned table holds.
    pub interned_strings: u64,
    /// The names the files are held under, each file's full path and
    /// the paths as written beside it, `ZCSG(hash).num_entries`.
    pub keys: u64,
    /// The files held, in `accelerator_get_scripts`' order, bucket by
    /// bucket of opcache's hash ([`crate::unit_cache::UnitCache::status`]).
    pub files: Vec<CachedFile>,
    /// What `opcache.preload` preloaded, `ZCSG(preload_script)`; none
    /// where nothing was.
    pub preload: Option<PreloadStatistics>,
    /// The compiled tier as the engine's tier reports it
    /// ([`crate::tier::Tier::status`]).
    pub tier: TierStatus,
}

/// `opcache_get_status()['jit']`, what php's `zend_jit_status` reports
/// (`Z/ext/opcache/jit/zend_jit.c:825-842`): `JIT_G` as php's start left
/// it, and the code buffer. A tier that compiles nothing reports every
/// field zero, the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierStatus {
    /// `JIT_G(enabled)`: `opcache.jit` set the tier up and php's start
    /// kept it.
    pub enabled: bool,
    /// `JIT_G(on)`: set up and on.
    pub on: bool,
    /// `JIT_G(trigger)`, the `T` of the `CRTO` digits: 5 for tracing.
    pub kind: u8,
    /// `JIT_G(opt_level)`, the `O` digit.
    pub opt_level: u8,
    /// `JIT_G(opt_flags)`: the `R` digit, with `ZEND_JIT_CPU_AVX`,
    /// `1<<2`, for a `C` of one.
    pub opt_flags: u8,
    /// `dasm_end - dasm_buf`: the buffer less the two pointers php keeps
    /// at its end; none where the tier was not set up.
    pub buffer_size: u64,
    /// `dasm_end - *dasm_ptr`: what of it no code takes yet.
    pub buffer_free: u64,
}

/// `opcache_get_status()['preload_statistics']`: what the preload
/// published for every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreloadStatistics {
    /// What the preload weighs, the bytes of the files it compiled.
    pub memory_consumption: u64,
    /// Each function it declared, by its name, in the order the
    /// function table holds them.
    pub functions: Vec<Vec<u8>>,
    /// Each row of its table of classes in order: a class by its name, an
    /// alias by its key, and a class that never linked by its name too.
    pub classes: Vec<Vec<u8>>,
    /// Each file it compiled, by its full path, in the order compiled.
    pub scripts: Vec<Vec<u8>>,
}

/// One file the cache holds, a row of `opcache_get_status()['scripts']`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFile {
    /// Its full path, the row's key and `full_path`.
    pub path: Vec<u8>,
    /// Requests served it from the cache since it was cached.
    pub hits: u64,
    /// The size of the file it was compiled from, in bytes.
    pub size: u64,
    /// When it was last served, in seconds.
    pub last_used: i64,
    /// The file's `mtime` when it was cached, in seconds; zero for a
    /// file cached without its timestamps validated.
    pub timestamp: i64,
    /// The request time up to which a hit is served without a stat,
    /// `dynamic_members.revalidate`; zero for a file cached without its
    /// timestamps validated.
    pub revalidate: i64,
}

/// The bounds of the unit cache's memory, which the extension that
/// configures it tells once ([`UnitCache::bound`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheBudget {
    /// `opcache.memory_consumption`, in bytes: what the files held and
    /// wasted may weigh together.
    pub memory: u64,
    /// `opcache.max_wasted_percentage`, a percentage of `memory`: what
    /// must be wasted for a cache that runs out to restart.
    pub max_wasted: u32,
    /// `ZCSG(hash).max_num_entries`: how many names files are held
    /// under, and the buckets they are held in.
    pub keys: u64,
    /// `ZCG(root_hash)`, which every name's hash is xored with before its
    /// bucket is taken: the inode of `/` where `opcache.validate_root`
    /// asks for it, and zero otherwise.
    pub root: u64,
    /// Whether the tier started in the segment (`zend_jit_startup`, in
    /// `accel_post_startup` once the segment is laid down), which takes
    /// the `zend_get_op_array_extension_handle` every op array compiled
    /// after it keeps a runtime cache slot for.
    pub tier: bool,
}

/// What [`UnitCache::reset`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    /// The cache empties at the next request's start.
    Scheduled,
    /// A restart was pending already, and nothing more is scheduled.
    Pending,
}

/// Which copies of a file [`UnitCache::invalidate`] discards,
/// `opcache_invalidate`'s `$force`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    /// The copy whose file changed since it was cached.
    Stale,
    /// Whatever the cache holds for the file.
    Forced,
}

impl Request {
    /// The one question the unit cache asks about the file at the full
    /// path `full` (design §6.3): the last supplier's answer, and with
    /// none, php without the cache.
    fn admission(&mut self, full: &[u8]) -> Admission {
        let shared = Arc::clone(&self.shared);
        match shared.supplied.admission.entry() {
            Some((by, hook)) => self.serve_for(by, |req| hook(req, full)),
            None => Admission::Held {
                validation: Validation::EveryHit,
                lookup: Lookup::Resolved,
                protection: 0,
                file_cache: None,
            },
        }
    }

    /// `accel_make_persistent_key`: the name a file included as `written`
    /// is filed under beside its full path, the path itself where it is
    /// absolute and, where it is relative, with the working directory,
    /// the `include_path` and the including file's directory that resolve
    /// it; none for an empty path or one a stream wrapper opens. A
    /// relative path's key asks for the working directory, which lays
    /// `ZCG(cwd)` down the first time ([`Request::accel_cwd`]).
    fn written_key(&mut self, written: &[u8]) -> Option<UnitKey> {
        if written.starts_with(b"/") || is_cacheable_stream_path(written) {
            return Some(UnitKey::full(written));
        }
        if written.is_empty() || super::paths::scheme_length(written).is_some() {
            return None;
        }
        // The memory limit's fatal is recorded and ends the request.
        self.accel_cwd().ok();
        Some(UnitKey::relative(
            written,
            self.cwd.as_os_str().as_encoded_bytes(),
            self.ini().get(b"include_path").unwrap_or_default(),
            self.frames().current_file(),
        ))
    }

    /// opcache's `persistent_zend_resolve_path` for the primary file,
    /// which leaves `persistent_stream_open_function` to answer its open
    /// without opening it: whether the accelerator is on for the request
    /// and holds a unit under the file's full path.
    pub(crate) fn accelerator_holds(&mut self, path: &[u8]) -> bool {
        let full = full_path(path, &self.cwd);
        let on = matches!(
            self.admission(&full),
            Admission::Held { validation, .. } if validation != Validation::EveryHit
        );
        let shared = Arc::clone(&self.shared);
        on && shared
            .units
            .find(&self.arena.files, &UnitKey::full(&full))
            .is_some()
    }

    /// `persistent_compile_file`'s first lookup, by the path as written:
    /// the file held under `written`'s name, where the answer looks by the
    /// path as written, and the validation the answer for it gives, which
    /// [`Request::held_answer`] holds it to where it is served. Nothing is
    /// validated or counted until then, so an `include_once` of a file
    /// already included is no hit, as `persistent_zend_resolve_path`
    /// counts none:
    ///
    /// ```php
    /// $f = __DIR__ . '/once.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// $hits = fn () => opcache_get_status()['opcache_statistics']['hits'];
    /// include $f; include $f; $before = $hits();
    /// var_dump(include_once $f, $hits() - $before);
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// bool(true)
    /// int(0)
    /// ```
    ///
    /// ```php
    /// ini_set('opcache.validate_timestamps', 0);
    /// $file = __DIR__ . '/gone.inc';
    /// file_put_contents($file, '<?php return 42;');
    /// var_dump(include $file);
    /// unlink($file);
    /// var_dump(include $file, include_once $file);
    /// ini_set('opcache.validate_timestamps', 1);
    /// var_dump(include $file, opcache_invalidate($file), @include $file);
    /// ```
    /// ```expect regime=jit-off
    /// int(42)
    /// int(42)
    /// bool(true)
    /// int(42)
    /// bool(true)
    /// bool(false)
    /// ```
    pub(crate) fn held_as_written(&mut self, written: &[u8]) -> Option<(Arc<Held>, Validation)> {
        let key = self.written_key(written)?;
        let shared = Arc::clone(&self.shared);
        let held = shared.units.find(&self.arena.files, &key)?;
        let Admission::Held {
            validation,
            lookup: Lookup::AsWritten,
            ..
        } = self.admission(&held.path)
        else {
            return None;
        };
        // The cache bypassed, `EveryHit`, looks nothing up by the key.
        (validation != Validation::EveryHit).then_some((held, validation))
    }

    /// `validate_timestamp_and_record` over `held`, found by the key the
    /// path `written` makes and handed over as `handle` holds it: fresh
    /// where nothing is due, and else as `do_validate_timestamps` finds it
    /// (`Z/ext/opcache/ZendAccelerator.c:1117-1187`), the path as written
    /// resolved again where `handle` has no opened path, without the
    /// file opened, so nothing guards it, a stream's to none
    /// (`php_resolve_path`), and the file stat'ed where the path is
    /// `held`'s. What a wrapper's `url_stat` throws on the way is thrown.
    fn held_fresh(
        &mut self,
        held: &Held,
        validation: Validation,
        written: &[u8],
        handle: HandlePath,
    ) -> Result<bool, Thrown> {
        let now = self.environment().request_time();
        if held.trusted(validation, now) {
            return Ok(true);
        }
        if handle.path_alone() && !is_wrapper_url(written) {
            match self.code_mut().resolve_path(written) {
                Ok(path) => {
                    self.revalidated_path(written, &path);
                    if path != held.path {
                        return Ok(false);
                    }
                }
                Err(Unresolved::Missing(_)) => {}
                Err(Unresolved::Threw(thrown)) => return Err(thrown),
            }
        }
        Ok(held.validated(validation, now, self))
    }

    /// What `held`, found by the key `written` makes under `validation`
    /// and handed over as `handle`, answers this request, as
    /// `persistent_compile_file` answers a hit by its key: held to
    /// [`Request::held_fresh`] and, fresh, a hit, counted, as
    /// [`crate::unit_cache::served`] serves one, the unit or the refusal
    /// with what it had declared made the request's own; none for a
    /// stale one, discarded as `zend_accel_lock_discard_script` discards
    /// it, which the miss after it opens and compiles.
    ///
    /// A hit whose stat is due opens nothing, so the `open_basedir` guard
    /// is asked only of a file found changed:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/due.inc'; file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// $g = realpath(__DIR__) . '/due2.inc'; file_put_contents($g, '<?php return 1;'); touch($g, time() - 60);
    /// var_dump(include $f, include $g, opcache_compile_file($f));
    /// file_put_contents($g, '<?php return 2;'); touch($g, time() - 30);
    /// ini_set('open_basedir', '/nonexistent');
    /// var_dump(include $f, opcache_compile_file($f), @include $g);
    /// var_dump(opcache_get_status()['scripts'][$g]['timestamp']);
    /// ```
    /// ```expect regime=jit-off
    /// int(1)
    /// int(1)
    /// bool(true)
    /// int(1)
    /// bool(true)
    /// bool(false)
    /// int(0)
    /// ```
    ///
    /// A key whose path now resolves to another held file is a miss, the
    /// file compiled and the other's copy served, no hit counted on it,
    /// and the key stays filed beside the discarded copy, so the next
    /// include by it is a miss again ([`Filed::Existing`]):
    ///
    /// ```php
    /// $d = realpath(__DIR__) . '/to' . getmypid(); @mkdir("$d/x", 0777, true); @mkdir("$d/y");
    /// foreach (['x', 'y'] as $t) { file_put_contents("$d/$t/a.inc", "<?php return '$t';"); touch("$d/$t/a.inc", time() - 60); }
    /// symlink("$d/x", "$d/to");
    /// $s = fn () => array_values(array_intersect_key(opcache_get_status()['opcache_statistics'], ['hits' => 0, 'misses' => 0]));
    /// echo include "$d/to/a.inc", include "$d/y/a.inc", include "$d/y/a.inc", "\n";
    /// [$h, $m] = $s();
    /// unlink("$d/to"); symlink("$d/y", "$d/to");
    /// var_dump(include "$d/to/a.inc", include_once "$d/to/a.inc", include "$d/to/a.inc");
    /// [$h2, $m2] = $s();
    /// echo $h2 - $h, ' ', $m2 - $m, ' ', opcache_get_status()['scripts']["$d/y/a.inc"]['hits'], "\n";
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect regime=jit-off
    /// xyy
    /// string(1) "y"
    /// bool(true)
    /// string(1) "y"
    /// 0 2 1
    /// ```
    ///
    /// and so it is under a file cache, which the miss does not read: the
    /// `zend_file_handle` holds the path alone, which
    /// `do_validate_timestamps` gives back unopened. Served by the binary, since the child is
    /// `PHP_BINARY`:
    ///
    /// ```php
    /// $d = realpath(sys_get_temp_dir()) . '/kept' . getmypid(); @mkdir("$d/x", 0777, true); @mkdir("$d/y"); @mkdir("$d/c");
    /// foreach (['x', 'y'] as $t) { file_put_contents("$d/$t/a.inc", "<?php return '$t';"); touch("$d/$t/a.inc", time() - 60); }
    /// symlink("$d/x", "$d/to");
    /// $code = '$d = $argv[1]; $s = fn () => opcache_get_status()["opcache_statistics"]; echo include "$d/to/a.inc", include "$d/y/a.inc"; $a = $s(); unlink("$d/to"); symlink("$d/y", "$d/to"); echo include "$d/to/a.inc", include "$d/to/a.inc"; $b = $s(); echo " ", $b["hits"] - $a["hits"], " ", $b["misses"] - $a["misses"], "\n";';
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.revalidate_freq=0', '-d', "opcache.file_cache=$d/c", '-r', $code, $d], [1 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]); proc_close($p);
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect regime=jit-off process
    /// xyyy 0 2
    /// ```
    pub(crate) fn held_answer(
        &mut self,
        held: &Held,
        validation: Validation,
        written: &[u8],
        handle: HandlePath,
    ) -> Result<Cached, Thrown> {
        if !self.held_fresh(held, validation, written, handle)? {
            self.shared.units.discard_held(&self.arena.files, held);
            return Ok(None);
        }
        let now = self.environment().request_time();
        self.arena.files.hit(held, validation, now);
        let answer = Some(Ok(PublishedUnit::clone(&held.unit)));
        let early = Some(Arc::clone(&held.early));
        self.note_load(&answer, validation, Filed::Hit, early);
        Ok(answer)
    }

    /// `persistent_zend_resolve_path` under `include_once` and
    /// `require_once`, the lookup by the key `written` makes having
    /// missed: the file held under the full path `path`, which the once
    /// form serves as one the key found, `ZCG(cache_persistent_script)`,
    /// and validates where it is served ([`Request::held_answer`]), with
    /// the key filed beside it as `zend_accel_add_key` files it, whether
    /// or not the file was included already, so the include that follows
    /// is a hit by the key and opens no stream. A relative include whose
    /// key is not filed yet while its full path is held opens the file:
    ///
    /// ```php
    /// function r() { return get_resource_id(fopen('php://memory', 'r')); }
    /// $dir = sys_get_temp_dir() . '/held' . getmypid(); @mkdir($dir); chdir($dir);
    /// foreach (['a', 'b', 'c'] as $f) { file_put_contents("$dir/$f.inc", "<?php return '$f';"); touch("$dir/$f.inc", time() - 10); }
    /// var_dump(r(), include "$dir/a.inc", r(), include 'a.inc', r(), include 'a.inc', r());
    /// var_dump(require "$dir/b.inc", r(), require_once './b.inc', r(), include './b.inc', r());
    /// var_dump(opcache_compile_file("$dir/c.inc"), r(), include_once 'c.inc', r(), include 'c.inc', r());
    /// array_map('unlink', glob("$dir/*.inc")); rmdir($dir);
    /// ```
    /// ```expect regime=jit-off
    /// int(8)
    /// string(1) "a"
    /// int(10)
    /// string(1) "a"
    /// int(12)
    /// string(1) "a"
    /// int(13)
    /// string(1) "b"
    /// int(15)
    /// bool(true)
    /// int(16)
    /// string(1) "b"
    /// int(17)
    /// bool(true)
    /// int(19)
    /// string(1) "c"
    /// int(20)
    /// string(1) "c"
    /// int(21)
    /// ```
    ///
    /// So a once form whose key names a discarded copy while the path
    /// it resolves to is held elsewhere is a hit on that file, where an
    /// include by the same key is a miss:
    ///
    /// ```php
    /// $d = realpath(__DIR__) . '/to' . getmypid(); @mkdir("$d/x", 0777, true); @mkdir("$d/y");
    /// foreach (['x', 'y'] as $t) { file_put_contents("$d/$t/a.inc", "<?php return '$t';"); touch("$d/$t/a.inc", time() - 60); }
    /// symlink("$d/x", "$d/to");
    /// $s = fn () => array_values(array_intersect_key(opcache_get_status()['opcache_statistics'], ['hits' => 0, 'misses' => 0]));
    /// var_dump(include "$d/to/a.inc", opcache_compile_file("$d/y/a.inc"));
    /// unlink("$d/to"); symlink("$d/y", "$d/to");
    /// var_dump(opcache_invalidate("$d/x/a.inc", true));
    /// [$h, $m] = $s();
    /// var_dump(include_once "$d/to/a.inc");
    /// [$h2, $m2] = $s();
    /// var_dump(include "$d/to/a.inc");
    /// [$h3, $m3] = $s();
    /// echo $h2 - $h, ' ', $m2 - $m, ' ', $h3 - $h2, ' ', $m3 - $m2, ' ', opcache_get_status()['scripts']["$d/y/a.inc"]['hits'], "\n";
    /// exec('rm -rf ' . escapeshellarg($d));
    /// ```
    /// ```expect regime=jit-off
    /// string(1) "x"
    /// bool(true)
    /// bool(true)
    /// string(1) "y"
    /// string(1) "y"
    /// 1 0 0 1 1
    /// ```
    pub(crate) fn file_once_key(
        &mut self,
        written: &[u8],
        path: &[u8],
    ) -> Option<(Arc<Held>, Validation)> {
        let full = full_path(path, &self.cwd);
        let Admission::Held {
            validation,
            lookup: Lookup::AsWritten,
            ..
        } = self.admission(&full)
        else {
            return None;
        };
        let shared = Arc::clone(&self.shared);
        let key = self.written_key(written)?;
        let held = shared
            .units
            .find(&self.arena.files, &UnitKey::full(&full))?;
        shared.units.add_key(&self.arena.files, key, &held);
        (validation != Validation::EveryHit).then_some((held, validation))
    }

    /// The unit for the file at `path`, resolved already, as
    /// `persistent_compile_file` gives it: a file the answer keeps out
    /// compiled as this request's own and counted among the blacklist
    /// misses, any other through the cache, validated as the answer
    /// says and filed under `written` too where the answer looks by the
    /// path as written.
    ///
    /// The file is opened as a stream, a resource id spent, wherever php
    /// opens it: to compile it, and to resolve its path after the lookup
    /// by the key missed, which a file read back from `opcache.file_cache`
    /// is found after, the key filed by the next include. The path as
    /// written carries a doubled slash, so it is never the resolved one.
    /// Served by the binary, since the child is `PHP_BINARY`:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '//kept' . getmypid(); @mkdir($dir);
    /// file_put_contents("$dir/a.inc", '<?php return 1;'); touch("$dir/a.inc", time() - 10);
    /// $code = 'function r() { return get_resource_id(fopen("php://memory", "r")); } echo implode(" ", [r(), include $argv[1], r(), include $argv[1], r(), include $argv[1], r()]), "\n";';
    /// foreach ([1, 2] as $run) {
    ///     $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', "opcache.file_cache=$dir", '-r', $code, "$dir/a.inc"], [1 => ['pipe', 'w']], $pipes);
    ///     echo stream_get_contents($pipes[1]); proc_close($p);
    /// }
    /// exec('rm -rf ' . escapeshellarg($dir));
    /// ```
    /// ```expect regime=jit-off process
    /// 5 1 7 1 8 1 9
    /// 5 1 7 1 9 1 10
    /// ```
    ///
    /// After `opcache_reset()` nothing new is held, so each include is a
    /// miss by the key that opens the file: compiled in the first child,
    /// and in the second read back from the file cache, which kept the
    /// first child's copy (the file changed since at the same `mtime`):
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/reset' . getmypid(); @mkdir($dir); $t = time() - 10;
    /// $code = 'function r() { return get_resource_id(fopen("php://memory", "r")); } echo implode(" ", [r(), opcache_reset() ? 1 : 0, r(), include $argv[1], r(), include $argv[1], r()]), "\n";';
    /// foreach ([1, 2] as $run) {
    ///     file_put_contents("$dir/a.inc", "<?php return $run;"); touch("$dir/a.inc", $t);
    ///     $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', "opcache.file_cache=$dir", '-r', $code, "$dir/a.inc"], [1 => ['pipe', 'w']], $pipes);
    ///     echo stream_get_contents($pipes[1]); proc_close($p);
    /// }
    /// exec('rm -rf ' . escapeshellarg($dir));
    /// ```
    /// ```expect regime=jit-off process
    /// 5 1 6 1 8 1 10
    /// 5 1 6 1 8 1 10
    /// ```
    pub(crate) fn cached_file(
        &mut self,
        path: &[u8],
        written: Option<&[u8]>,
        handle: HandlePath,
    ) -> Cached {
        self.cached_source(FileArgs::new(path, written, handle))
    }

    /// [`Request::cached_file`] where a `zend_compile_file` link may have
    /// handed down a `source` to compile under the file's path in place
    /// of the file's own, the engine's cached compile beneath every link
    /// ([`crate::api::abi::chain::CompileFile`]): what is compiled from
    /// it is held under the file's names as the file's own would be, as
    /// opcache's `persistent_compile_file` caches what the compile it
    /// saved, phar's, made of an archive's stub. The link opened the
    /// stream it read, so the compile opens none
    /// ([`FileArgs::opens_stream`]).
    ///
    /// ```php
    /// function member($name, $data) {
    ///     $h = pack('a100a8a8a8a12a12a8a1a100a6a2a247', $name, '0000644', '', '', sprintf('%011o', strlen($data)), '00000000000', '        ', '0', '', 'ustar', '00', '');
    ///     $h = substr_replace($h, sprintf('%06o', array_sum(array_map('ord', str_split($h)))) . "\0 ", 148, 8);
    ///     return $h . str_pad($data, (strlen($data) + 511) & ~511, "\0");
    /// }
    /// $t = __DIR__ . '/cached.phar.tar';
    /// $tar = member('.phar/stub.php', '<?php echo "stub\n"; __HALT_COMPILER();');
    /// $tar .= member('.phar/signature.bin', pack('VV', 2, 20) . sha1($tar, true)) . str_repeat("\0", 1024);
    /// file_put_contents($t, $tar); touch($t, time() - 60);
    /// $s = fn () => opcache_get_status()['opcache_statistics'];
    /// $before = $s();
    /// include $t; include $t;
    /// $after = $s();
    /// var_dump(opcache_is_script_cached($t), $after['hits'] - $before['hits'], $after['misses'] - $before['misses'], $after['num_cached_scripts'] - $before['num_cached_scripts']);
    /// unlink($t);
    /// ```
    /// ```expect regime=jit-off
    /// stub
    /// stub
    /// bool(true)
    /// int(1)
    /// int(1)
    /// int(1)
    /// ```
    ///
    /// A compile that runs makes its runtime definition keys and
    /// anonymous class names on from the request's `CG(rtd_key_counter)`
    /// and leaves them counted there, as opcache's miss compiles with
    /// what the request made before it; a hit compiles nothing and counts nothing,
    /// its unit keeping the names its miss persisted:
    ///
    /// ```php
    /// $inc = __DIR__ . '/anonymous.inc'; file_put_contents($inc, "<?php\nreturn new class {};\n"); touch($inc, time() - 10);
    /// $n = fn ($o) => strstr(get_class($o), '$');
    /// echo $n(eval('return new class {};')), ' ', $n(include $inc), ' ', $n(eval('return new class {};')), ' ', $n(include $inc), ' ', $n(eval('return new class {};')), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// $0 $1 $2 $1 $3
    /// ```
    pub(crate) fn cached_source(&mut self, args: FileArgs<'_>) -> Cached {
        let FileArgs {
            path,
            written,
            handle,
            source,
            ..
        } = args;
        if self.preloading.is_some() {
            return self.preload_file(path);
        }
        let shared = Arc::clone(&self.shared);
        let full = full_path(path, &self.cwd);
        if !args.cached() {
            return self.compiled_alone(args, &full);
        }
        let (validation, lookup, protection, file_cache, memory) = match self.admission(&full) {
            Admission::KeptOut => return self.kept_out(args, &full),
            Admission::Held {
                validation,
                lookup,
                protection,
                file_cache,
            } => (validation, lookup, protection, file_cache, true),
            Admission::OnDisk {
                validation,
                protection,
                file_cache,
            } => (
                validation,
                Lookup::Resolved,
                protection,
                Some(file_cache),
                false,
            ),
        };
        let now = self.environment().request_time();
        let resolved = lookup == Lookup::Resolved;
        let written = (written.filter(|_| lookup == Lookup::AsWritten))
            .and_then(|written| self.written_key(written));
        let levels = levels(file_cache.as_ref(), memory, &shared.units.build);
        // `persistent_compile_file` opens the file to resolve its path
        // wherever the lookup by the key misses.
        let by_key = written
            .as_ref()
            .is_some_and(|key| shared.units.find(&self.arena.files, key).is_some());
        let asked = Asked {
            path: &full,
            source,
            written,
            opened: !handle.path_alone(),
            validation,
            protection,
            now,
            levels,
            optimizer: shared.optimizer(),
        };
        let arena = Arc::clone(&self.arena);
        // What a compile that ran read and made, none for a hit.
        let scanned = std::cell::Cell::new(None);
        let (compiled, filed, early) = shared.units.file(
            self,
            &arena.files,
            asked,
            |req, source| {
                let compiled = req.compile_for_cache(source, path);
                scanned.set(Some(Scanned::of(source, &compiled.unit)));
                compiled
            },
            |body| shared.decode_kept(body),
            |unit| shared.publish_compiled(unit),
            |req, unit| req.publish_unit(unit),
        );
        if filed == Filed::Own {
            return self.compiled_alone(args, &full);
        }
        let scanned = scanned.get();
        self.note_pushed(scanned.map_or_else(super::Pushed::default, |scanned| scanned.pushed));
        let cached = crate::unit_cache::served(compiled, |unit| self.take_unit(unit));
        self.count_keys_made(keys_counted(scanned, validation, &cached));
        // The primary file compiled through the host's handle, which
        // `zend_stream_fixup` reads it into and `CG(open_files)` lists.
        if handle == HandlePath::Host
            && let Some(scanned) = scanned
        {
            self.fill_primary_handle(scanned.len).ok();
        }
        if matches!(cached, Some(Ok(_))) {
            self.tell_compiled(&full, validation, filed);
        }
        self.note_load(&cached, validation, filed, early);
        // The file opened as a stream where php compiles it or resolves
        // it to look it up by its full path, and not for a hit whose key
        // the cache files: one by the full path, or read back from the
        // file cache, after the key missed is opened all the same.
        if args.opens_stream() && cached.is_some() && (filed != Filed::Hit || resolved || !by_key) {
            self.open_include_stream(&full, handle);
        }
        cached
    }

    /// `source`, read from the file `path` names, compiled for the cache
    /// at this request's asking ([`crate::engine::Shared::compile_file`]): under the
    /// request's encoding and with its keys and classes, as opcache's miss
    /// compiles on the request, and charged to the request's heap while
    /// it runs. Since php 8.5.12 a fold past `memory_limit` is served, the
    /// delayed OOM named at the `include` that compiles it, and the compile
    /// runs on; one that outgrows the hard limit, twice the soft one, ends
    /// the process there with php's line on its standard error and
    /// nothing else served, no shutdown function among it, with opcache and
    /// without it. The request holds nothing of the compile once it
    /// ends, and a hit compiles nothing. The ask that crosses the hard
    /// limit is the fold's 16007168 bytes, as opcache's miss asks it;
    /// php's own compile without opcache crosses one fold step sooner,
    /// at 15007744, which the engine's compile, always opcache's, does
    /// not follow, so the first fence leaves the figure open:
    ///
    /// ```php
    /// ini_set('display_errors', '0');
    /// register_shutdown_function(function () {
    ///     $e = error_get_last();
    ///     echo $e ? $e['type'] . ' ' . strtok($e['message'], '(') . 'on line ' . $e['line'] : 'no error', "\n";
    ///     var_dump(function_exists('f'), class_exists('C', false));
    /// });
    /// $a = str_repeat('a', 1000000);
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// $file = sys_get_temp_dir() . '/fold-' . getmypid() . '.php';
    /// file_put_contents($file, "<?php\nfunction f() {}\nclass C {\n    const A = '$a';\n    const X = $x;\n}\n");
    /// ini_set('memory_limit', '16M');
    /// register_shutdown_function(fn () => @unlink($file));
    /// include $file;
    /// echo "included\n";
    /// ```
    /// ```expect process exit=137
    /// ```
    /// ```stderr
    /// Hard memory limit of 33554432 bytes exhausted (tried to allocate %d bytes) in %s on line 14
    /// ```
    /// ```php
    /// ini_set('display_errors', '0');
    /// register_shutdown_function(function () {
    ///     $e = error_get_last();
    ///     echo $e ? $e['type'] . ' ' . strtok($e['message'], '(') . 'on line ' . $e['line'] : 'no error', "\n";
    ///     var_dump(function_exists('f'), class_exists('C', false));
    /// });
    /// $a = str_repeat('a', 1000000);
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// $file = sys_get_temp_dir() . '/fold-' . getmypid() . '.php';
    /// file_put_contents($file, "<?php\nfunction f() {}\nclass C {\n    const A = '$a';\n    const X = $x;\n}\n");
    /// ini_set('memory_limit', '16M');
    /// register_shutdown_function(fn () => @unlink($file));
    /// include $file;
    /// echo "included\n";
    /// ```
    /// ```expect regime=jit-off process exit=137
    /// ```
    /// ```stderr
    /// Hard memory limit of 33554432 bytes exhausted (tried to allocate 16007168 bytes) in %s on line 14
    /// ```
    ///
    /// The limit holds what php's compile holds at once, the string a fold
    /// makes beside the operands it reads, each given back once the node
    /// over it stands (`php_compile`'s fold claims): the file compiles
    /// under 40M and not under 32M, as php's does in either regime. Since
    /// php 8.5.12 the compile runs on past the limit, which it meets as
    /// the delayed OOM the included file's first safepoint raises at the
    /// `include`'s line, and under 16M on to the hard limit, where the
    /// child ends with nothing written:
    ///
    /// ```php
    /// $child = <<<'PHP'
    /// <?php
    /// ini_set('display_errors', '0');
    /// register_shutdown_function(function () {
    ///     $e = error_get_last();
    ///     echo $e ? strtok($e['message'], '(') . 'on line ' . $e['line'] : strlen(C::X) . ' bytes', "\n";
    /// });
    /// $a = str_repeat('a', 1000000);
    /// $x = implode(' . ', array_fill(0, 16, 'self::A'));
    /// $file = sys_get_temp_dir() . '/fold-inc-' . getmypid() . '.php';
    /// file_put_contents($file, "<?php\nclass C {\n    const A = '$a';\n    const X = $x;\n}\n");
    /// ini_set('memory_limit', $argv[1]);
    /// register_shutdown_function(fn () => @unlink($file));
    /// include $file;
    /// PHP;
    /// $f = sys_get_temp_dir() . '/fold-' . getmypid() . '.php';
    /// file_put_contents($f, $child);
    /// foreach (['16M', '24M', '32M', '40M', '128M'] as $limit) {
    ///     foreach (['-n', '-n -d opcache.enable_cli=1 -d opcache.jit=off -d opcache.file_update_protection=0'] as $regime) {
    ///         $out = shell_exec(escapeshellarg(PHP_BINARY) . " $regime " . escapeshellarg($f) . " $limit 2>&1");
    ///         echo $limit, ' ', preg_replace('/ \(tried to allocate .*/', '', $out);
    ///     }
    /// }
    /// unlink($f);
    /// ```
    /// ```expect process
    /// 16M Hard memory limit of 33554432 bytes exhausted
    /// 16M Hard memory limit of 33554432 bytes exhausted
    /// 24M Allowed memory size of 25165824 bytes exhausted on line 13
    /// 24M Allowed memory size of 25165824 bytes exhausted on line 13
    /// 32M Allowed memory size of 33554432 bytes exhausted on line 13
    /// 32M Allowed memory size of 33554432 bytes exhausted on line 13
    /// 40M 16000000 bytes
    /// 40M 16000000 bytes
    /// 128M 16000000 bytes
    /// 128M 16000000 bytes
    /// ```
    pub(crate) fn compile_for_cache(&mut self, source: &[u8], path: &[u8]) -> Fresh {
        let shared = Arc::clone(&self.shared);
        let stack = self.stack_limit();
        let bound = crate::multibyte::bound(self, shared.compile_multibyte());
        let keys_before = self.keys_made();
        self.compiling_for_cache(|heap, classes, cwd| {
            // opcache compiles the file under
            // `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`, so it substitutes
            // none of the request's constants.
            let request = ForRequest {
                classes,
                constants: None,
                multibyte: bound.as_ref().map(crate::multibyte::Bound::settings),
                keys_before,
            };
            shared.compile_file(
                source,
                path,
                cwd,
                stack,
                request,
                CompileHeap::Request(heap),
            )
        })
    }

    /// What `zend_accel_load_script` is told of a file the cache answered
    /// as `filed`: how opcache served it, the classes its compiler bound
    /// early, published as it was held, which its load declares by their
    /// numbers, and, for a unit opcache loaded, the note that its classes
    /// are immutable, since php keeps a link in its inheritance cache only
    /// under opcache (`ZEND_ACC_IMMUTABLE`).
    fn note_load(
        &mut self,
        cached: &Cached,
        validation: Validation,
        filed: Filed,
        early: Option<crate::unit_cache::EarlyBound>,
    ) {
        let served = AccelServed::of(validation, filed);
        self.accel_loaded = (cached.clone().and_then(Result::ok)).zip(served);
        self.early_bound = cached
            .clone()
            .and_then(Result::ok)
            .zip(early)
            .filter(|(_, early)| early.iter().any(Option::is_some));
        if let (Some(Ok(unit)), Filed::Hit | Filed::Held | Filed::Existing, Some(_)) =
            (cached, filed, served)
        {
            self.note_held(unit);
        }
    }

    /// A file the answer keeps out of the cache, `blacklist_misses`
    /// counted, compiled as this request's own ([`Request::compiled_alone`]).
    fn kept_out(&mut self, args: FileArgs<'_>, full: &[u8]) -> Cached {
        self.arena.files.note_blacklisted();
        self.compiled_alone(args, full)
    }

    /// A file no cache holds compiled as this request's own, from the
    /// `source` handed down or the file's bytes, which are opened as a
    /// stream where php opens them ([`FileArgs::opens_stream`]): one the
    /// answer keeps out, and a wrapper's stream opcache does not cache
    /// ([`FileArgs::cached`]).
    ///
    /// And one the cache leaves to it ([`crate::unit_cache::Filed::Own`]):
    /// written within `opcache.file_update_protection` seconds, which
    /// opcache compiles through `accelerator_orig_compile_file` before it
    /// would persist anything, so the include costs what php's compile
    /// lays on the request, its names made as an `eval`'s are
    /// ([`Request::hold_unit_names`]) and its file's name the string
    /// `EG(included_files)` keeps, a once form's put there before the
    /// compile ([`Request::opened_once`]) (the program under the recipe's
    /// `TMPDIR`, read after the heap is churned):
    ///
    /// ```php
    /// $put = function ($name, $src) { $f = sys_get_temp_dir() . "/$name"; file_put_contents("$f.tmp", $src); touch("$f.tmp", time() - 10); rename("$f.tmp", $f); return $f; };
    /// $f = $put('w3316-fresh-include.php', <<<'PHP'
    /// <?php
    /// ini_set('opcache.file_update_protection', '2');
    /// function inc($name, $src, $once = false) { $f = sys_get_temp_dir() . "/w3316-$name.inc"; file_put_contents($f, $src); $m = memory_get_usage(); $once ? include_once $f : include $f; $d = memory_get_usage() - $m; unlink($f); return $d; }
    /// inc('w0', '<?php class W {} $q0 = 1;');
    /// echo inc('a', '<?php function abcdefgz() { return 2; } $GLOBALS["d"] = function () { $abcdefgz = "w"; return $abcdefgz; };'), "\n";
    /// echo inc('b', '<?php class Abcdefgy { public $abcdefgy = 1; protected $Abcdefgy = 2; function Abcdefgy2() { return __FUNCTION__ . __METHOD__ . __CLASS__; } }'), "\n";
    /// echo inc('c', '<?php $GLOBALS["e"] = function () { return __FUNCTION__ . __FUNCTION__; }; $GLOBALS["g"] = fn() => __FUNCTION__ . __FUNCTION__;'), "\n";
    /// echo inc('d', '<?php if (1) { function Qwertyz() { $qwertyz = 1; return [$qwertyz, __FUNCTION__]; } }'), "\n";
    /// echo inc('e', '<?php function Fqwerty1() { return 1; }', true), "\n";
    /// $junk = array_fill(0, 1000, str_repeat("x", 100));
    /// echo json_encode([abcdefgz(), $d(), (array) new Abcdefgy, (new Abcdefgy)->Abcdefgy2(), $e() === $g(), Qwertyz(), Fqwerty1()]), "\n";
    /// $m = memory_get_usage(); unset($d, $e, $g); echo memory_get_usage() - $m, "\n";
    /// PHP);
    /// include $f;
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 1400
    /// 1008
    /// 1616
    /// 384
    /// 272
    /// [2,"w",{"abcdefgy":1,"\u0000*\u0000Abcdefgy":2},"Abcdefgy2Abcdefgy::Abcdefgy2Abcdefgy",true,[1,"Qwertyz"],1]
    /// -2192
    /// ```
    fn compiled_alone(&mut self, args: FileArgs<'_>, full: &[u8]) -> Cached {
        let source = crate::unit_cache::source_or_read(args.source, args.path)?;
        if args.opens_stream() {
            self.open_include_stream(full, args.handle);
        }
        if args.handle == HandlePath::Host {
            self.fill_primary_handle(source.len()).ok();
        }
        if let Some(opened) = self.opened_once(full) {
            self.compile_opened(opened);
        }
        let shared = Arc::clone(&self.shared);
        Some(shared.compile_for(self, &source, args.path, SourceKind::File))
    }

    /// `do_validate_timestamps` over a held copy whose stat is due
    /// (`Z/ext/opcache/ZendAccelerator.c:1117`): the `zend_file_handle`
    /// has no opened path, so the path as written is resolved again
    /// (`accelerator_orig_zend_resolve_path`), and the request pays for
    /// `tsrm_realpath`'s buffer, laid down as `emalloc(1)` for an
    /// absolute path or a copy of the working directory for a relative
    /// one and grown to the resolved path by `virtual_file_ex`, beside
    /// the `zend_string` `tsrm_realpath_str` copies it into, the buffer
    /// freed first and the string after the stat. A heap that refuses
    /// one has recorded the memory limit's fatal, which ends the request
    /// at the next step; nothing else reads it.
    ///
    /// The included file's op array and its runtime cache are the
    /// request's while it runs ([`crate::exec::HeapOpArray`]), which is the
    /// whole of a short path's peak; past about a hundred bytes of path
    /// the two strings are more:
    ///
    /// ```php
    /// function peak($f) {
    ///     file_put_contents($f, '<?php $x = 1; return $x;'); touch($f, time() - 10); clearstatcache();
    ///     include $f; include $f;
    ///     $s = memory_get_usage(); memory_reset_peak_usage(); include $f;
    ///     echo memory_get_peak_usage() - $s, "\n"; unlink($f);
    /// }
    /// peak('/tmp/' . bin2hex(random_bytes(8)) . '.php');
    /// peak(__DIR__ . '/' . str_repeat('v', 150 - strlen(__DIR__)) . '.php');
    /// peak(__DIR__ . '/' . str_repeat('w', 230 - strlen(__DIR__)) . '.php');
    /// ```
    /// ```expect regime=jit-off
    /// 272
    /// 352
    /// 576
    /// ```
    fn revalidated_path(&mut self, written: &[u8], full: &[u8]) {
        let start = if written.first() == Some(&b'/') {
            1
        } else {
            self.cwd.as_os_str().len().saturating_add(1)
        };
        let Ok(buffer) = Emalloc::new(self, start) else {
            return;
        };
        let Ok(buffer) = buffer.resize(self, full.len().saturating_add(1)) else {
            return;
        };
        let resolved = PhpString::filled(self, full.len(), |out| out.copy_from_slice(full));
        buffer.free(self);
        if let Ok(resolved) = resolved {
            resolved.give_up(self);
        }
    }

    /// `filename_is_in_file_cache`: whether `file_cache` keeps a unit of
    /// the file `written` resolves to, as `zend_file_cache_script_load_ex`
    /// would read it back, the file's `mtime` held to the one it was
    /// kept at where the answer for it validates timestamps; what a
    /// wrapper's `url_stat` throws on the way is thrown.
    fn kept_on_disk(&mut self, written: &[u8], file_cache: &FileCache) -> Result<bool, Thrown> {
        let full = match self.code_mut().resolve_path(written) {
            Ok(full) => full,
            Err(Unresolved::Missing(_)) => return Ok(false),
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
        };
        let validation = match self.admission(&full) {
            Admission::KeptOut => Validation::Never,
            Admission::Held { validation, .. } | Admission::OnDisk { validation, .. } => validation,
        };
        let stamp = match validation {
            Validation::Never => None,
            Validation::Window { .. } | Validation::EveryHit => match self.stamp(&full) {
                Some(stamp) => Some(stamp.seconds()),
                None => return Ok(false),
            },
        };
        Ok(disk(file_cache, &self.shared.units.build).holds(&full, stamp))
    }

    /// The file cache the answer for the file at the full path `full`
    /// names, where it names one.
    fn file_cache_of(&mut self, full: &[u8]) -> Option<FileCache> {
        match self.admission(full) {
            Admission::KeptOut => None,
            Admission::Held { file_cache, .. } => file_cache,
            Admission::OnDisk { file_cache, .. } => Some(file_cache),
        }
    }

    /// What follows a file the cache compiled as `filed`: the passes
    /// called where `cache_script_in_shared_memory` and
    /// `cache_script_in_file_cache` optimize what they store, before they
    /// store it, and the extensions told where the first holds it, which
    /// opcache logs cached. A file validated on every hit is one no
    /// accelerator admitted, held as php without opcache compiles it,
    /// which optimizes nothing.
    fn tell_compiled(&mut self, full: &[u8], validation: Validation, filed: Filed) {
        if validation != Validation::EveryHit
            && matches!(filed, Filed::Held | Filed::Existing | Filed::Kept)
        {
            self.run_optimizer_passes(full);
        }
        if filed == Filed::Held {
            self.tell_cached(full);
        }
    }

    /// Every pass an extension registered called with the file at the
    /// full path `full`, in the order they were registered
    /// ([`crate::api::abi::ExtensionBuilder::optimizer_pass`]),
    /// `zend_optimizer_call_registered_passes` at the end of
    /// `zend_optimize_script`.
    pub(crate) fn run_optimizer_passes(&mut self, full: &[u8]) {
        tell!(self, optimizer_passes, |req, pass| pass(req, full));
    }

    /// Every extension told of a file the unit cache holds by its full
    /// path, in bundle order ([`crate::api::abi::ExtensionBuilder::cached`]).
    pub(crate) fn tell_cached(&mut self, full: &[u8]) {
        tell!(self, cached, |req, hook| hook(req, full));
    }
}

impl Stat for Request {
    /// `zend_get_file_handle_timestamp`: a plain file's `stat(2)`, and a
    /// stream wrapper's path's `zend_get_stream_timestamp`, its wrapper's
    /// `url_stat` asked quietly through the stream layer
    /// ([`crate::layer::StreamHooks::timestamp`]).
    fn stamp(&mut self, path: &[u8]) -> Option<Stamp> {
        if !is_wrapper_url(path) {
            return Stamp::of(path);
        }
        let hooks = self.stream_hooks()?;
        (hooks.timestamp)(self, path).map(Stamp::stat)
    }
}

impl<'r> Code<&'r mut Request> {
    /// The unit cache (design §6.3), as `opcache_*` read it.
    pub fn unit_cache(self) -> UnitCache<'r> {
        UnitCache { req: self.0 }
    }
}

impl UnitCache<'_> {
    /// The cache's counters and scripts.
    #[must_use]
    pub fn status(&self) -> CacheStatus {
        let req = &self.req;
        let tier = (req.shared.loops.status)(req);
        (req.shared.units).status(&req.arena.files, req.shared.interned_strings(), tier)
    }

    /// `filename_is_in_cache`: whether a unit is held under the name
    /// `written` is filed under, as an include files it, and is fresh as
    /// an include's hit by the key is ([`Request::held_fresh`]), a stale
    /// one discarded by nothing. With `on_disk`, the question is
    /// the second level's, `filename_is_in_file_cache`: whether that file
    /// cache keeps a unit of the file `written` resolves to that it would
    /// read back, its `mtime` held to the one it was kept at where the
    /// answer for the file validates timestamps.
    pub fn holds(&mut self, written: &[u8], on_disk: Option<&FileCache>) -> Result<bool, Thrown> {
        let req = &mut *self.req;
        if let Some(file_cache) = on_disk {
            return req.kept_on_disk(written, file_cache);
        }
        let Some(held) = req
            .written_key(written)
            .and_then(|key| req.shared.units.find(&req.arena.files, &key))
        else {
            return Ok(false);
        };
        let validation = match req.admission(&held.path) {
            Admission::KeptOut | Admission::OnDisk { .. } => return Ok(false),
            Admission::Held { validation, .. } => validation,
        };
        req.held_fresh(&held, validation, written, HandlePath::Filename)
    }

    /// `zend_accel_invalidate`: the copies `how` names of the file
    /// `written` names discarded, the path resolved along the
    /// `include_path` and taken as written where it resolves to nothing,
    /// so a deleted file is invalidated still, and whatever a wrapper's
    /// `url_stat` throws thrown. Whether the file was found, on disk or
    /// in the cache.
    ///
    /// A discarded copy stays filed, as php's `corrupted` entry stays in
    /// its hash: listed with its timestamp zeroed and counted, but cached
    /// no more, until an include compiles the file again in its place.
    /// The count is read past what the cache held before the include,
    /// which leaves out the primary file whether or not it is held:
    ///
    /// ```php
    /// $count = fn() => opcache_get_status()['opcache_statistics']['num_cached_scripts'];
    /// $f = realpath(sys_get_temp_dir()) . '/inv' . getmypid() . '.inc';
    /// file_put_contents($f, '<?php return 1;'); touch($f, time() - 60);
    /// $before = $count();
    /// include $f;
    /// var_dump(opcache_invalidate($f, true), opcache_invalidate($f, true));
    /// $s = opcache_get_status();
    /// var_dump(isset($s['scripts'][$f]), $s['scripts'][$f]['timestamp'], $count() - $before, opcache_is_script_cached($f));
    /// var_dump(include $f, opcache_is_script_cached($f));
    /// $s = opcache_get_status();
    /// var_dump($count() - $before, $s['scripts'][$f]['hits'], $s['scripts'][$f]['timestamp'] > 0);
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// int(0)
    /// int(1)
    /// bool(false)
    /// int(1)
    /// bool(true)
    /// int(1)
    /// int(0)
    /// bool(true)
    /// ```
    pub fn invalidate(&mut self, written: &[u8], how: Invalidation) -> Result<bool, Thrown> {
        let (path, on_disk) = match self.req.code_mut().resolve_path(written) {
            Ok(path) => (path, true),
            Err(Unresolved::Missing(_)) => (written.to_vec(), false),
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
        };
        if let Some(file_cache) = self.req.file_cache_of(&path) {
            disk(&file_cache, &self.req.shared.units.build).invalidate(&path);
        }
        let (shared, arena) = (Arc::clone(&self.req.shared), Arc::clone(&self.req.arena));
        let forced = how == Invalidation::Forced;
        let held = (shared.units).discard(&arena.files, &UnitKey::full(&path), forced, self.req);
        Ok(on_disk || held)
    }

    /// `opcache_compile_file`'s `persistent_compile_file` without the
    /// execution: the file `written` names served by the key as an
    /// `include` serves it ([`Request::held_answer`]), opened by nothing,
    /// and else resolved as an `include` resolves it and held to the
    /// `open_basedir` guard as its open is, failing with the include's
    /// warnings under the native's name,
    /// compiled into the cache, and its functions and the
    /// classes it can bind declared in the request, which leaves
    /// `get_included_files()` as it was. `false` where nothing opens; a
    /// parse error is the `ParseError` an include throws, and a refusal
    /// after the parse ends the request.
    pub fn compile(&mut self, written: &[u8]) -> Result<bool, Thrown> {
        let req = &mut *self.req;
        let nul = written.contains(&0);
        let served = match (!nul).then(|| req.held_as_written(written)).flatten() {
            Some((held, how)) => (req.held_answer(&held, how, written, HandlePath::Filename)?)
                .map(|unit| (unit, held.path.clone())),
            None => None,
        };
        // The plain files opener's guard, which warns under the native's
        // name: of the resolved path, and of the path as written where
        // none resolved; the stream's warning names the same.
        let opened = match served {
            Some(served) => Ok(served),
            None if nul => Err((written.to_vec(), unreadable(c_str(written)))),
            None => match req.code_mut().resolve_path(written) {
                Ok(path) => match req.environment_mut().check_open_basedir(&path, true) {
                    Ok(_) => match req.cached_file(&path, Some(written), HandlePath::Filename) {
                        Some(unit) => Ok((unit, path)),
                        None => Err((written.to_vec(), unreadable(&path))),
                    },
                    Err(refused) => Err((path, refused.into())),
                },
                Err(Unresolved::Missing(reason)) => {
                    let refused = req.environment_mut().check_open_basedir(written, true);
                    Err((written.to_vec(), refused.err().map_or(reason, Into::into)))
                }
                Err(Unresolved::Threw(thrown)) => return Err(thrown),
            },
        };
        let unit = match opened {
            Ok((Ok(unit), _)) => unit,
            Ok((Err(refused), path)) => {
                return Err(req.refused_include(&refused, &path, php_compile::SourceKind::File));
            }
            Err((named, reason)) => {
                let failed = req.not_opened(IncludeKind::Include, &named, &reason, written);
                return failed.map(|_| false);
            }
        };
        if !crate::engine::note_compiled(req, &unit, php_compile::SourceKind::File) {
            return Err(Thrown::of(None));
        }
        Ok(true)
    }

    /// `zend_accel_schedule_restart(ACCEL_RESTART_USER)`, what
    /// `opcache_reset` asks: the request's epoch replaced at the next
    /// request's start, which is handed an empty cache and an empty
    /// published arena (design §5.14), and until then what is compiled
    /// is held nowhere.
    ///
    /// The restart zeroes the hits, the misses and the blacklist misses,
    /// `zend_reset_cache_vars`, so the request after it counts its own
    /// program's miss alone. Served by the built-in server, whose
    /// requests share the cache:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/zero' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/held.inc", '<?php return 1;');
    /// file_put_contents("$dir/out.inc", '<?php return 2;');
    /// file_put_contents("$dir/list.txt", "$dir/out.inc\n");
    /// file_put_contents("$dir/index.php", '<?php $c = fn () => array_intersect_key(opcache_get_status(false)["opcache_statistics"], ["hits" => 0, "misses" => 0, "blacklist_misses" => 0]);
    /// if ($_GET["step"] == "a") {
    ///     include __DIR__ . "/held.inc"; include __DIR__ . "/held.inc"; include __DIR__ . "/out.inc";
    ///     echo json_encode($c()), "\n";
    ///     var_dump(opcache_reset());
    /// } else {
    ///     echo json_encode($c()), "\n";
    /// }');
    /// foreach (['held.inc', 'out.inc', 'index.php'] as $f) { touch("$dir/$f", time() - 60); }
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-d', "opcache.blacklist_filename=$dir/list.txt", '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $ready = [$pipes[2]]; $none = null;
    /// $line = stream_select($ready, $none, $none, 30) ? fgets($pipes[2]) : '';
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach (['a', 'b'] as $step) { echo file_get_contents("http://127.0.0.1:$port/?step=$step"); }
    /// proc_terminate($p); proc_close($p);
    /// exec('rm -rf ' . escapeshellarg($dir));
    /// ```
    /// ```expect process
    /// {"hits":1,"misses":2,"blacklist_misses":1}
    /// bool(true)
    /// {"hits":0,"misses":1,"blacklist_misses":0}
    /// ```
    pub fn reset(&self) -> Restart {
        self.req.arena.files.schedule_restart()
    }

    /// The cache's bounds, told once as the cache starts
    /// (`zend_accel_init_shm`): a file that does not fit is held
    /// nowhere, and a cache that runs out restarts once what it wasted
    /// reaches `max_wasted` percent of its memory.
    pub fn bound(&self, budget: CacheBudget) {
        self.req.shared.units.set_budget(budget);
    }

    /// `CG(map_ptr_last)`: how many map pointers the engine has handed
    /// the op arrays of the units it published, one each as php's
    /// `zend_persist_op_array` takes one (design §5.14). A unit compiled
    /// for one request alone takes none, so a count that moves between
    /// two requests of one engine serving the same file is a leak of
    /// slots (GH-8646).
    #[must_use]
    pub fn map_ptr_last(&self) -> u32 {
        (self.req.shared.map_ptr_last).load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The file cache `file_cache` names, for the build `build`.
fn disk<'a>(file_cache: &'a FileCache, build: &'a [u8; crate::unit_cache::BUILD]) -> Disk<'a> {
    Disk {
        directory: &file_cache.directory,
        read_only: file_cache.read_only,
        build,
    }
}

/// What a compile [`Request::cached_source`] ran read and made: the
/// source's length, which the primary file's `zend_stream_fixup` reads
/// it into, the most php's compiler stacks held ([`super::Pushed`]),
/// and the runtime
/// definition keys an accepted unit made, which opcache's miss leaves
/// counted in `CG(rtd_key_counter)`. A refusal's keys are counted as it
/// is raised ([`Request::declare_refused`]), so none are here.
#[derive(Clone, Copy)]
struct Scanned {
    len: usize,
    pushed: super::Pushed,
    keys: u32,
}

/// The runtime definition keys the request counts for the file served
/// as `cached` under `validation`, where `scanned` says what a compile
/// that ran made: the keys an accepted unit made, none for a refusal,
/// whose keys are counted as it is raised ([`Request::declare_refused`]),
/// and a hit's ([`crate::unit_cache::hit_keys`]).
fn keys_counted(scanned: Option<Scanned>, validation: Validation, cached: &Cached) -> u32 {
    match (scanned, cached) {
        (Some(scanned), _) => scanned.keys,
        (None, Some(Ok(unit))) => crate::unit_cache::hit_keys(validation, unit),
        (None, _) => 0,
    }
}

impl Scanned {
    /// What compiling `source` to `compiled` read and made.
    fn of(source: &[u8], compiled: &Result<php_unit::Unit, php_compile::CompileError>) -> Scanned {
        let unit = compiled.as_ref().ok();
        Scanned {
            len: source.len(),
            pushed: unit.map_or_else(super::Pushed::default, super::Pushed::of),
            keys: unit.map_or(0, crate::unit_cache::keys_of),
        }
    }
}

/// The levels a lookup asks: shared memory where `memory`, the file
/// cache where one is configured, or both.
fn levels<'a>(
    file_cache: Option<&'a FileCache>,
    memory: bool,
    build: &'a [u8; crate::unit_cache::BUILD],
) -> Levels<'a> {
    match file_cache.map(|file_cache| disk(file_cache, build)) {
        None => Levels::Memory,
        Some(disk) if memory => Levels::Both(disk),
        Some(disk) => Levels::Disk(disk),
    }
}

impl AccelServed {
    /// How `zend_accel_load_script` serves a file
    /// [`crate::unit_cache::UnitCache::file`] filed: it loads what
    /// opcache held or kept, compiled by this request where its miss
    /// held it, and none of what opcache compiled as php without it.
    fn of(validation: Validation, filed: Filed) -> Option<AccelServed> {
        match filed {
            _ if validation == Validation::EveryHit => None,
            Filed::Compiled | Filed::Own => None,
            Filed::Held | Filed::Existing => Some(AccelServed::Compiled),
            Filed::Hit | Filed::Unheld | Filed::Kept => Some(AccelServed::Hit),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::{Arc, mpsc};

    use super::{Request, Restart};
    use crate::engine::{Engine, RequestDescription};
    use crate::unit_cache::UnitKey;

    /// A file whose class constant folds sixteen copies of a megabyte.
    fn folding() -> String {
        let a = "a".repeat(1_000_000);
        let x = vec!["self::A"; 16].join(" . ");
        format!("<?php\nclass C {{\n    const A = '{a}';\n    const X = {x};\n}}\n")
    }

    /// A compile bound for the cache is charged to the heap of the
    /// request that asked while it runs: a `memory_limit` that refuses it
    /// answers that request alone and the cache holds nothing, so the
    /// next request, with room for it, compiles it and the cache holds
    /// that; and a compile, refused or not, leaves the request's usage
    /// where it stood.
    #[test]
    fn a_cache_bound_compile_refused_by_the_asking_request_s_limit_is_held_for_no_other() {
        let files = crate::test::Files::new("charged", &[("fold.php", &folding())]);
        let file = files.path().join("fold.php");
        let resolved = std::fs::canonicalize(&file).unwrap_or_default();
        let path = resolved.as_os_str().as_encoded_bytes().to_vec();
        let source = std::fs::read(&file).unwrap_or_default();
        let engine = Engine::builder().build();
        let start = |limit: &str| {
            let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
            let code = format!("ini_set('memory_limit', '{limit}');");
            assert!(
                req.code_mut()
                    .eval_stringl(code.as_bytes(), b"limit")
                    .is_ok()
            );
            req
        };
        let held = |req: &Request| {
            (engine.shared.units)
                .find(&req.arena.files, &UnitKey::full(&path))
                .is_some()
        };
        let class = |req: &Request| req.class_named(php_unit::decl::Bare::resolved(b"c"));
        let code = format!("require '{}';", file.display());

        for (limit, answers) in [
            ("16M", crate::unit_cache::Answers::Asker),
            ("128M", crate::unit_cache::Answers::Every),
        ] {
            let mut req = start(limit);
            let before = req.heap.memory_usage(false);
            let fresh = req.compile_for_cache(&source, &path);
            assert_eq!(fresh.answers, answers, "under {limit}");
            assert_eq!(
                fresh.unit.is_ok(),
                answers == crate::unit_cache::Answers::Every
            );
            assert_eq!(
                req.heap.memory_usage(false),
                before,
                "nothing stands under {limit}"
            );
        }

        // Refused again by the next request under the same limit, as
        // php's compile is where opcache cached nothing.
        for _ in 0..2 {
            let mut refused = start("16M");
            refused
                .code_mut()
                .eval_stringl(code.as_bytes(), b"loader")
                .ok();
            assert!(class(&refused).is_none(), "the fold's fatal declared no C");
            assert!(
                !held(&refused),
                "a refusal of the asker's alone is held for no other"
            );
        }
        let mut roomy = start("128M");
        assert!(
            roomy
                .code_mut()
                .eval_stringl(code.as_bytes(), b"loader")
                .is_ok()
        );
        assert!(class(&roomy).is_some());
        assert!(held(&roomy), "held once a request with room compiled it");
    }

    /// `accel_activate`'s restart with no wait for `accel_is_inactive`: a
    /// chain of requests on two threads, each started before the one
    /// before it ends, so no instant has none running. A restart
    /// scheduled in the middle hands the next request a new epoch, its
    /// arena empty, while the request that scheduled it keeps its own
    /// arena and sees the restart pending; the old arena is dropped when
    /// that last holder ends.
    #[test]
    fn a_restart_hands_the_next_request_a_new_epoch_while_the_old_one_runs() {
        let engine = Engine::builder().build();
        let start = &|| Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let units = &engine.shared.units;
        let (to_second, at_second) = mpsc::channel::<()>();
        let (to_first, at_first) = mpsc::channel::<()>();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let first = start();
                assert!(first.arena.reserve_class().is_some());
                to_second.send(()).ok();
                at_first.recv().ok();
                drop(first);
                let third = start();
                assert_eq!(third.arena.counts(), [0, 0, 0], "an empty arena");
                assert!(!third.arena.files.restart_pending());
                assert_eq!(
                    units
                        .status(&third.arena.files, 0, super::TierStatus::default())
                        .manual_restarts,
                    1
                );
                to_second.send(()).ok();
                at_first.recv().ok();
                drop(third);
            });
            scope.spawn(move || {
                at_second.recv().ok();
                let second = start();
                assert_eq!(second.arena.counts(), [1, 0, 0], "the first's epoch");
                assert_eq!(second.arena.files.schedule_restart(), Restart::Scheduled);
                assert_eq!(second.arena.files.schedule_restart(), Restart::Pending);
                to_first.send(()).ok();
                at_second.recv().ok();
                assert_eq!(second.arena.counts(), [1, 0, 0], "its rows stand");
                assert!(second.arena.files.restart_pending());
                let held = Arc::downgrade(&second.arena);
                drop(second);
                assert!(held.upgrade().is_none(), "dropped with its last holder");
                to_first.send(()).ok();
            });
        });
        assert_eq!(
            units
                .status(
                    &engine.shared.arena.current().files,
                    0,
                    super::TierStatus::default()
                )
                .manual_restarts,
            1
        );
    }
}

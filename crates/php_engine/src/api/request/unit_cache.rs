//! The unit cache as a request reads and moves it (design §6.3): what
//! `opcache_get_status`, `opcache_reset`, `opcache_invalidate`,
//! `opcache_compile_file` and `opcache_is_script_cached` answer from,
//! since 8.5 makes opcache always present. The cache is the engine's and
//! shared by every request (`crate::unit_cache`); an extension reaches it
//! through [`Request::unit_cache`] and nothing else, and decides for
//! itself whether its configuration lets it answer, as opcache's
//! `accelerator_enabled` does.

use std::sync::Arc;

use php_compile::SourceKind;

use super::Request;
use crate::api::raising::Thrown;
use crate::exec::IncludeKind;
use crate::unit_cache::{Cached, UnitKey};

/// The unit cache for one request: [`Request::unit_cache`].
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
    /// The strings the engine's interned table holds.
    pub interned_strings: u64,
    /// The files held, in the order the cache took them.
    pub files: Vec<CachedFile>,
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
    /// The file's `mtime` when it was cached, in seconds.
    pub timestamp: i64,
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
    /// The unit cache (design §6.3), as `opcache_*` read it.
    pub fn unit_cache(&mut self) -> UnitCache<'_> {
        UnitCache { req: self }
    }

    /// The unit for the file at `path`, resolved already, as
    /// `persistent_compile_file` gives it: a file an extension's
    /// blacklist names compiled as this request's own and counted among
    /// the blacklist misses, any other through the cache.
    pub(crate) fn cached_file(&mut self, path: &[u8]) -> Cached {
        let shared = Arc::clone(&self.shared);
        let blacklisted = shared
            .extensions
            .iter()
            .filter_map(|extension| extension.blacklist)
            .any(|hook| hook(self, path));
        if blacklisted {
            shared.units.note_blacklisted();
            let source = std::fs::read(String::from_utf8_lossy(path).as_ref()).ok()?;
            return Some(shared.compile_for(self, &source, path, SourceKind::File));
        }
        let cwd = self.cwd.clone();
        let stack = self.stack_limit();
        shared.cached(path, &cwd, stack)
    }
}

impl UnitCache<'_> {
    /// The cache's counters and scripts.
    #[must_use]
    pub fn status(&self) -> CacheStatus {
        let shared = &self.req.shared;
        shared.units.status(shared.interned_strings())
    }

    /// `filename_is_in_cache`: whether the file `written` names, resolved
    /// along the `include_path` as an include resolves it, is held as it
    /// stands now.
    pub fn holds(&mut self, written: &[u8]) -> bool {
        let Ok(path) = self.req.resolve_path(written) else {
            return false;
        };
        let cwd = self.req.cwd.clone();
        UnitKey::of(&path, &cwd).is_some_and(|key| self.req.shared.units.holds(&key))
    }

    /// `zend_accel_invalidate`: the copies `how` names of the file
    /// `written` names discarded, the path resolved along the
    /// `include_path` and taken as written where it resolves to nothing,
    /// so a deleted file is invalidated still. Whether the file was
    /// found, on disk or in the cache.
    pub fn invalidate(&mut self, written: &[u8], how: Invalidation) -> bool {
        let resolved = self.req.resolve_path(written).ok();
        let on_disk = resolved.is_some();
        let path = resolved.unwrap_or_else(|| written.to_vec());
        let cwd = self.req.cwd.clone();
        let current = UnitKey::of(&path, &cwd);
        let held = self.req.shared.units.discard(
            current.as_ref().map_or(&path[..], UnitKey::path),
            current.as_ref(),
            how == Invalidation::Forced,
        );
        on_disk || held
    }

    /// `opcache_compile_file`'s `persistent_compile_file` without the
    /// execution: the file `written` names resolved as an `include`
    /// resolves it, failing with the include's two warnings under the
    /// native's name, compiled into the cache, and its functions and the
    /// classes it can bind declared in the request, which leaves
    /// `get_included_files()` as it was. `false` where nothing opens; a
    /// parse error is the `ParseError` an include throws, and a refusal
    /// after the parse ends the request.
    pub fn compile(&mut self, written: &[u8]) -> Result<bool, Thrown> {
        let req = &mut *self.req;
        let resolved = if written.contains(&0) {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        } else {
            req.resolve_path(written)
        };
        let unit = match resolved.map(|path| (req.cached_file(&path), path)) {
            Ok((Some(Ok(unit)), _)) => unit,
            Ok((Some(Err(refused)), path)) => return Err(req.refused_include(&refused, &path)),
            Ok((None, _)) => {
                let reason = std::io::Error::from(std::io::ErrorKind::NotFound);
                req.open_stream_failed(IncludeKind::Include, written, &reason);
                return req
                    .failed_opening(IncludeKind::Include, written)
                    .map(|_| false);
            }
            Err(reason) => {
                if !written.contains(&0) {
                    req.open_stream_failed(IncludeKind::Include, written, &reason);
                }
                return req
                    .failed_opening(IncludeKind::Include, written)
                    .map(|_| false);
            }
        };
        crate::engine::note_compiled(req, &unit);
        if !req.declare_classes(&unit) || !req.declare_functions(&unit) {
            return Err(Thrown::of(None));
        }
        Ok(true)
    }

    /// `zend_accel_schedule_restart(ACCEL_RESTART_USER)`, what
    /// `opcache_reset` asks: the cache empties at the next request's
    /// start, and until then what is compiled is held nowhere.
    pub fn reset(&self) -> Restart {
        self.req.shared.units.schedule_restart()
    }
}

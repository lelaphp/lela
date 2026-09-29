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

use super::{Request, Unresolved};
use crate::api::abi::{Admission, Lookup, Validation};
use crate::api::raising::Thrown;
use crate::exec::IncludeKind;
use crate::unit_cache::{Cached, Held, UnitKey, full_path};

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
    /// The names the files are held under, each file's full path and
    /// the paths as written beside it, `ZCSG(hash).num_entries`.
    pub keys: u64,
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
    /// The file's `mtime` when it was cached, in seconds; zero for a
    /// file cached without its timestamps validated.
    pub timestamp: i64,
    /// The request time up to which a hit is served without a stat,
    /// `dynamic_members.revalidate`; zero for a file cached without its
    /// timestamps validated.
    pub revalidate: i64,
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

    /// The one question the unit cache asks about the file at the full
    /// path `full` (design §6.3): the first extension's answer in bundle
    /// order, and with none, php without the cache.
    fn admission(&mut self, full: &[u8]) -> Admission {
        let shared = Arc::clone(&self.shared);
        match shared
            .extensions
            .iter()
            .find_map(|extension| extension.admission)
        {
            Some(hook) => hook(self, full),
            None => Admission::Held {
                validation: Validation::EveryHit,
                lookup: Lookup::Resolved,
                protection: 0,
            },
        }
    }

    /// `accel_make_persistent_key`: the name a file included as `written`
    /// is filed under beside its full path, the path itself where it is
    /// absolute and, where it is relative, with the working directory,
    /// the `include_path` and the including file's directory that resolve
    /// it; none for an empty path or one a stream wrapper opens.
    fn written_key(&self, written: &[u8]) -> Option<UnitKey> {
        if written.is_empty() || super::scheme_length(written).is_some() {
            return None;
        }
        if written.starts_with(b"/") {
            return Some(UnitKey::full(written));
        }
        Some(UnitKey::relative(
            written,
            self.cwd.as_os_str().as_encoded_bytes(),
            self.ini_get(b"include_path").unwrap_or_default(),
            self.current_file(),
        ))
    }

    /// `persistent_compile_file`'s first lookup, by the path as written:
    /// the file held under `written`'s name, a hit served without the
    /// path resolved or the file stat'ed, where the answer for it looks
    /// by the path as written and nothing is due; none where a hit would
    /// need either, which the lookup by the full path gives.
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
    pub(crate) fn held_as_written(&mut self, written: &[u8]) -> Option<Arc<Held>> {
        let key = self.written_key(written)?;
        let shared = Arc::clone(&self.shared);
        let held = shared.units.find(&key)?;
        let Admission::Held {
            validation,
            lookup: Lookup::AsWritten,
            ..
        } = self.admission(&held.path)
        else {
            return None;
        };
        let now = self.request_time();
        if !held.trusted(validation, now) {
            return None;
        }
        shared.units.hit(&held, validation, now);
        Some(held)
    }

    /// The unit for the file at `path`, resolved already, as
    /// `persistent_compile_file` gives it: a file the answer keeps out
    /// compiled as this request's own and counted among the blacklist
    /// misses, any other through the cache, validated as the answer
    /// says and filed under `written` too where the answer looks by the
    /// path as written.
    pub(crate) fn cached_file(&mut self, path: &[u8], written: Option<&[u8]>) -> Cached {
        let shared = Arc::clone(&self.shared);
        let full = full_path(path, &self.cwd);
        let (validation, lookup, protection) = match self.admission(&full) {
            Admission::KeptOut => {
                shared.units.note_blacklisted();
                let source = std::fs::read(String::from_utf8_lossy(path).as_ref()).ok()?;
                return Some(shared.compile_for(self, &source, path, SourceKind::File));
            }
            Admission::Held {
                validation,
                lookup,
                protection,
            } => (validation, lookup, protection),
        };
        let written = match lookup {
            Lookup::AsWritten => written.and_then(|written| self.written_key(written)),
            Lookup::Resolved => None,
        };
        let now = self.request_time();
        let cwd = self.cwd.clone();
        let stack = self.stack_limit();
        shared
            .units
            .file(&full, written, validation, protection, now, |source| {
                shared.publish_file(source, path, &cwd, stack)
            })
    }
}

impl UnitCache<'_> {
    /// The cache's counters and scripts.
    #[must_use]
    pub fn status(&self) -> CacheStatus {
        let shared = &self.req.shared;
        shared.units.status(shared.interned_strings())
    }

    /// `filename_is_in_cache`: whether a unit is held under the name
    /// `written` is filed under, as an include files it, and is fresh as
    /// the answer for it validates a hit, which stats nothing where no
    /// hit is due; one that is due is held to the file `written`
    /// resolves to as `do_validate_timestamps` holds it, which throws
    /// what a wrapper's `url_stat` throws.
    pub fn holds(&mut self, written: &[u8]) -> Result<bool, Thrown> {
        let req = &mut *self.req;
        let Some(held) = req
            .written_key(written)
            .and_then(|key| req.shared.units.find(&key))
        else {
            return Ok(false);
        };
        if held.unit.is_err() {
            return Ok(false);
        }
        let validation = match req.admission(&held.path) {
            Admission::KeptOut => return Ok(false),
            Admission::Held { validation, .. } => validation,
        };
        let now = req.request_time();
        if held.trusted(validation, now) {
            return Ok(true);
        }
        match req.resolve_path(written) {
            Ok(path) if path != held.path => return Ok(false),
            Ok(_) | Err(Unresolved::Missing(_)) => {}
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
        }
        Ok(held.validated(validation, now))
    }

    /// `zend_accel_invalidate`: the copies `how` names of the file
    /// `written` names discarded, the path resolved along the
    /// `include_path` and taken as written where it resolves to nothing,
    /// so a deleted file is invalidated still, and whatever a wrapper's
    /// `url_stat` throws thrown. Whether the file was found, on disk or
    /// in the cache.
    pub fn invalidate(&mut self, written: &[u8], how: Invalidation) -> Result<bool, Thrown> {
        let (path, on_disk) = match self.req.resolve_path(written) {
            Ok(path) => (path, true),
            Err(Unresolved::Missing(_)) => (written.to_vec(), false),
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
        };
        let held = self
            .req
            .shared
            .units
            .discard(&UnitKey::full(&path), how == Invalidation::Forced);
        Ok(on_disk || held)
    }

    /// `opcache_compile_file`'s `persistent_compile_file` without the
    /// execution: the file `written` names resolved as an `include`
    /// resolves it and held to the `open_basedir` guard as its open is,
    /// failing with the include's warnings under the native's name,
    /// compiled into the cache, and its functions and the
    /// classes it can bind declared in the request, which leaves
    /// `get_included_files()` as it was. `false` where nothing opens; a
    /// parse error is the `ParseError` an include throws, and a refusal
    /// after the parse ends the request.
    pub fn compile(&mut self, written: &[u8]) -> Result<bool, Thrown> {
        let req = &mut *self.req;
        let (resolved, held) = if written.contains(&0) {
            let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
            (Err(Unresolved::Missing(missing)), None)
        } else {
            match req.held_as_written(written) {
                Some(held) => (Ok(held.path.clone()), Some(held.unit.clone())),
                None => (req.resolve_path(written), None),
            }
        };
        // The plain files opener's guard, which warns under the native's
        // name: of the resolved path, or the held unit's, and of the path
        // as written where none resolved.
        let resolved = match resolved {
            Ok(path) => match req.check_open_basedir(&path, true) {
                Ok(_) => Ok(path),
                Err(refused) => {
                    req.open_stream_failed(IncludeKind::Include, &path, &refused.into());
                    return req
                        .failed_opening(IncludeKind::Include, written)
                        .map(|_| false);
                }
            },
            Err(Unresolved::Missing(reason)) if !written.contains(&0) => {
                match req.check_open_basedir(written, true) {
                    Ok(_) => Err(Unresolved::Missing(reason)),
                    Err(refused) => Err(Unresolved::Missing(refused.into())),
                }
            }
            other => other,
        };
        let compiled = resolved.map(|path| {
            let unit = held.or_else(|| req.cached_file(&path, Some(written)));
            (unit, path)
        });
        let unit = match compiled {
            Ok((Some(Ok(unit)), _)) => unit,
            Ok((Some(Err(refused)), path)) => return Err(req.refused_include(&refused, &path)),
            Ok((None, _)) => {
                let reason = std::io::Error::from(std::io::ErrorKind::NotFound);
                req.open_stream_failed(IncludeKind::Include, written, &reason);
                return req
                    .failed_opening(IncludeKind::Include, written)
                    .map(|_| false);
            }
            Err(Unresolved::Threw(thrown)) => return Err(thrown),
            Err(Unresolved::Missing(reason)) => {
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

//! `opcache.preload` as the unit cache answers it (design §6.3), the
//! request's half: `accel_preload`'s request, which compiles every file
//! into the preload rather than the cache and binds no class the file
//! was only compiled for; `preload_link`, which links what that left
//! unlinked and warns of what it cannot; and the tables every request
//! after starts from, php's `preload_load` into the process's tables.
//! The preload is published once and read by every request, the tables
//! in the order php appends them: the classes as `preload_sort_classes`
//! leaves the preload's table of classes, and the functions reversed, as
//! `preload_move_user_functions` walks the function table from its end.
//! Its classes and functions are the first rows of the engine's
//! published arena (design §5.14, *Identities*), so every request names
//! them by the same numbers and reads them with nothing taken.

use std::collections::HashMap;
use std::sync::Arc;

use php_link::{Composed, Folded, FoldedHash, Linked, NameMap, Obligation};
use php_unit::decl::{Bare, ClassDecl, Name, shown};
use php_unit::frozen::Frozen;
use php_unit::{AutoGlobal, Level, PublishedUnit};

use super::Code;
use super::Frames;
use super::publish::Unpublished;
use super::{InArena, PreloadStatistics, Request, UserFunction, user_function_decl};
use crate::api::call::Function;
use crate::api::values::ClassId;
use crate::arena::EpochClassId;
use crate::engine::Cached;
use crate::exec::Body;
use crate::unit_cache::{Held, full_path};

/// Where each key of a table stands, any case: the row, counted as
/// [`Keyed::get`] counts it.
type Keys = NameMap<Folded, usize>;

/// The preload's rows of a keyed table with their keys, shared by every
/// request as the preload's table is: php's `CG(class_table)` as
/// `preload_load` left it, which each request's starts from.
pub(crate) struct Rows<T> {
    rows: Arc<[T]>,
    keys: Arc<Keys>,
}

impl<T> Rows<T> {
    /// The rows in their order.
    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[T] {
        &self.rows
    }

    /// The rows, each under its key, the first row of a key kept, as
    /// `zend_hash_add` refuses a key the table holds.
    fn of(keyed: Vec<(Box<[u8]>, T)>) -> Rows<T> {
        let mut keys = Keys::new();
        let mut rows = Vec::with_capacity(keyed.len());
        for (at, (key, row)) in keyed.into_iter().enumerate() {
            keys.add(&key, at);
            rows.push(row);
        }
        Rows {
            rows: rows.into(),
            keys: Arc::new(keys),
        }
    }
}

impl<T> Default for Rows<T> {
    fn default() -> Rows<T> {
        Rows {
            rows: Arc::new([]),
            keys: Arc::default(),
        }
    }
}

/// A table of `EG(class_table)`, found by key in one hash lookup, as
/// `zend_hash_find` finds a bucket, and walked in the order its rows
/// were added: the preload's rows and keys, shared with every request,
/// then the request's own, dropped with it. A row is what the table
/// files under the name, never the class's own row, which is the
/// arena's or the request's (design §5.14, *Identities*).
pub(crate) struct Keyed<T> {
    preloaded: Arc<[T]>,
    preloaded_keys: Arc<Keys>,
    own: Vec<T>,
    own_keys: Keys,
}

impl<T> Keyed<T> {
    /// A table whose first rows are the preload's.
    fn over(preloaded: &Rows<T>) -> Keyed<T> {
        Keyed {
            preloaded: Arc::clone(&preloaded.rows),
            preloaded_keys: Arc::clone(&preloaded.keys),
            own: Vec::new(),
            own_keys: Keys::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.preloaded.len() + self.own.len()
    }

    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        match at.checked_sub(self.preloaded.len()) {
            None => self.preloaded.get(at),
            Some(own) => self.own.get(own),
        }
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.preloaded.iter().chain(self.own.iter())
    }

    /// The row filed under `key`, any case.
    pub(crate) fn find(&self, key: &[u8]) -> Option<&T> {
        self.find_hashed(FoldedHash::of(key), key)
    }

    /// [`Keyed::find`] by the hash `key` was taken at.
    pub(crate) fn find_hashed(&self, hash: FoldedHash, key: &[u8]) -> Option<&T> {
        let at = *self
            .preloaded_keys
            .find_hashed(hash, key)
            .or_else(|| self.own_keys.find_hashed(hash, key))?;
        self.get(at)
    }

    /// `row` filed under `key`, which the table must not hold yet in any
    /// case: a second row of a key is never found by it.
    pub(crate) fn push(&mut self, key: &[u8], row: T) {
        let at = self.len();
        self.own_keys.add(key, at);
        self.own.push(row);
    }

    /// The request's own rows and their keys, given up.
    fn clear_own(&mut self) {
        self.own.clear();
        self.own_keys.clear();
    }
}

impl<T> Default for Keyed<T> {
    fn default() -> Keyed<T> {
        Keyed::over(&Rows::default())
    }
}

/// A class's row of `EG(class_table)`: the bucket it stands in and the
/// class it names, the arena's or the request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) place: Bucket,
    pub(crate) class: ClassId,
}

/// An alias row of the table of classes: the bucket it stands in, the
/// lowered name, and the class it names.
pub(crate) type Alias = (Bucket, Vec<u8>, ClassId);

/// A bucket of `EG(class_table)` past the engine's classes: where a
/// class or an alias stands, in the order php appends its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Bucket(usize);

/// A request's buckets: the next to hand out, and those a unit's load
/// took for the classes it declares under runtime definition keys, as
/// `zend_compile_class_decl` adds each under its key in the order
/// written; the bind that renames the key (`zend_hash_set_bucket_key`)
/// claims its bucket.
#[derive(Default)]
pub(crate) struct Buckets {
    next: usize,
    reserved: Vec<Reserved>,
}

/// The bucket a load took for the class at `at` of `unit`.
struct Reserved {
    unit: PublishedUnit,
    at: u32,
    bucket: Bucket,
}

impl Buckets {
    /// Buckets past the `rows` the preload left.
    pub(crate) fn past(rows: usize) -> Buckets {
        Buckets {
            next: rows,
            reserved: Vec::new(),
        }
    }

    /// The next bucket, taken.
    pub(crate) fn take(&mut self) -> Bucket {
        self.next += 1;
        Bucket(self.next - 1)
    }

    /// The next bucket, held for the class at `at` of `unit`.
    pub(crate) fn reserve(&mut self, unit: &PublishedUnit, at: u32) {
        let bucket = self.take();
        let unit = PublishedUnit::clone(unit);
        self.reserved.push(Reserved { unit, at, bucket });
    }

    /// The bucket held for the class at `at` of `unit`, or the next.
    pub(crate) fn claim(&mut self, unit: &PublishedUnit, at: u32) -> Bucket {
        let held =
            (self.reserved.iter()).position(|r| PublishedUnit::ptr_eq(&r.unit, unit) && r.at == at);
        match held {
            Some(i) => self.reserved.swap_remove(i).bucket,
            None => self.take(),
        }
    }
}

/// What the preload left for every request (design §6.3):
/// `ZCSG(preload_script)` and `ZCSG(saved_scripts)`. Built once by the
/// preload's request and read by every request after; nothing in it
/// changes but the scripts' hit counters.
pub(crate) struct Preloaded {
    /// The preload's `EG(class_table)`: each class in its bucket under
    /// its name, found in any case, its row the engine's published
    /// arena's, which the preload is the first to publish into (design
    /// §5.14).
    pub(crate) classes: Rows<Named>,
    pub(crate) aliases: Rows<Alias>,
    /// The preload's functions, published, in the order its
    /// `EG(function_table)` holds them, each under its name.
    pub(crate) functions: Rows<Function>,
    /// `ping_auto_globals_mask`: the superglobals the preloaded code
    /// names, armed at every request's start (`preload_activate`).
    pub(crate) arms: Vec<AutoGlobal>,
    /// Each file the preload compiled, in the order compiled: php's
    /// saved scripts, held in the cache, never validated and outliving
    /// every restart.
    pub(crate) scripts: Vec<Arc<Held>>,
    /// Each `eval`'s unit of which the preload keeps a class or a
    /// function, in the order compiled, saved as no file of its own and
    /// laid with the preloaded files below every epoch, as php's
    /// `$PRELOAD$` holds an `eval()'d code`'s op arrays beside the files'; an
    /// `eval` whose declarations it keeps none of is dropped as the
    /// preload ends.
    pub(crate) evals: Vec<Frozen<PublishedUnit>>,
    /// Which declarations of which unit the preload holds, by the unit's
    /// address, which the preload keeps alive for the engine's life.
    declared: HashMap<usize, UnitDeclarations>,
    pub(crate) statistics: PreloadStatistics,
}

/// The classes and functions of one preloaded unit the preload holds:
/// what `preload_remove_declares` turned the declaring oplines of into
/// `NOP`s, and what a saved file's empty tables leave undeclared.
#[derive(Default)]
struct UnitDeclarations {
    classes: Vec<u32>,
    functions: Vec<Body>,
}

impl UnitDeclarations {
    /// Whether the preload keeps any declaration of the unit: the one
    /// place it is asked, which keeps an `eval`'s unit past the preload.
    fn keeps(&self) -> bool {
        !(self.classes.is_empty() && self.functions.is_empty())
    }
}

impl Preloaded {
    /// Whether the class at `at` of `unit` is one the preload holds.
    pub(crate) fn holds_class(&self, unit: &PublishedUnit, at: u32) -> bool {
        self.declared
            .get(&address(unit))
            .is_some_and(|held| held.classes.contains(&at))
    }

    /// Whether the function `body` of `unit` is one the preload holds.
    pub(crate) fn holds_function(&self, unit: &PublishedUnit, body: Body) -> bool {
        self.declared
            .get(&address(unit))
            .is_some_and(|held| held.functions.contains(&body))
    }

    /// The saved file at the full path `path`.
    pub(crate) fn script(&self, path: &[u8]) -> Option<&Arc<Held>> {
        self.scripts.iter().find(|held| held.path == path)
    }
}

/// A unit's identity for as long as the preload keeps it.
fn address(unit: &PublishedUnit) -> usize {
    PublishedUnit::as_ptr(unit) as usize
}

/// The preload's request: the files it compiled, in order, and how
/// many aliases the extensions' `RINIT`s had added before its program
/// began, which are theirs and every request's, not the preload's.
pub(crate) struct Preloading {
    /// Each a unit [`Frozen::new`] admitted: a file whose unit it
    /// refuses is served to the preload as its own and saved nowhere.
    scripts: Vec<(Vec<u8>, Frozen<PublishedUnit>, u64)>,
    /// Each `eval`'s unit [`Frozen::new`] admitted, whose classes and
    /// functions the preload publishes as php's
    /// `preload_move_user_classes` keeps an `eval()'d code`'s of a
    /// preloaded file, and which the preload saves as no file of its own.
    evals: Vec<Frozen<PublishedUnit>>,
    aliases_before: usize,
    /// `ZEND_COMPILE_PRELOAD_IN_CHILD` ([`Code::preloading_in_child`]).
    in_child: bool,
}

/// One class `preload_link` asks about: a top-level or an anonymous
/// class of a preloaded file that nothing bound.
struct Candidate {
    unit: PublishedUnit,
    at: u32,
    decl: ClassDecl,
    linked: bool,
    /// The last refusal its link met, with the site php names.
    refused: Option<(String, Vec<u8>, u32)>,
}

impl Preloading {
    /// The unit the preload froze that `unit` is, a preloaded file's or
    /// an `eval`'s; none for one the request holds as its own.
    fn frozen(&self, unit: &PublishedUnit) -> Option<&Frozen<PublishedUnit>> {
        (self.scripts.iter().map(|(_, frozen, _)| frozen))
            .chain(&self.evals)
            .find(|frozen| PublishedUnit::ptr_eq(frozen, unit))
    }
}

/// A row of the preload's table of classes, `preload_sort_classes`'s bucket:
/// a class the request bound, an alias `class_alias()` added, or a class
/// that never linked, which php keeps under its runtime definition key
/// and lists by its name.
enum Row {
    Class(ClassId),
    Alias(Vec<u8>, ClassId),
    Unlinked(Vec<u8>),
}

impl Request {
    /// The request, started, is the preload's from here: every file it
    /// compiles is compiled into the preload. `in_child` where it is
    /// served on the thread that took `opcache.preload_user`'s
    /// credentials, php's `ZEND_COMPILE_PRELOAD_IN_CHILD`.
    pub(crate) fn begin_preload(&mut self, in_child: bool) {
        self.preloading = Some(Preloading {
            scripts: Vec::new(),
            evals: Vec::new(),
            aliases_before: self.aliases.len(),
            in_child,
        });
    }

    /// `preload_compile_file` over the file at `path`: compiled as php
    /// compiles it with the accelerator off, recorded among the
    /// preload's scripts, and the unit already compiled for the path
    /// served again; nothing where the file cannot be read.
    pub(crate) fn preload_file(&mut self, path: &[u8]) -> Cached {
        let full = full_path(path, &self.cwd);
        if let Some((_, unit, _)) = self
            .preloading
            .as_ref()?
            .scripts
            .iter()
            .find(|(held, _, _)| *held == full)
        {
            return Some(Ok(PublishedUnit::clone(unit)));
        }
        let source = std::fs::read(String::from_utf8_lossy(path).as_ref()).ok()?;
        let shared = Arc::clone(&self.shared);
        // Optimized as the preload's whole, after every script compiled
        // (`preload_optimize`), and not as each compiles.
        let unit = (self.compile_for_cache(&source, path, crate::unit_cache::Optimizer::Off)).unit;
        if let Ok(unit) = &unit {
            self.count_keys_made(crate::unit_cache::keys_of(unit));
        }
        // The preload optimizes what it holds (`zend_optimize_script`
        // in `preload_optimize`).
        let optimizer = shared.optimizer();
        let unit = unit.map(|unit| shared.publish_compiled(optimizer.optimized(unit)));
        if let (Ok(Ok(unit)), Some(preloading)) = (&unit, &mut self.preloading) {
            let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
            preloading.scripts.push((full, Frozen::clone(unit), size));
        }
        let unit = unit.map(|unit| unit.map_or_else(|own| own, |unit| PublishedUnit::clone(&unit)));
        Some(unit.map_err(|error| {
            crate::unit_cache::CompileRefused::published(error, |unit| self.take_unit(unit))
        }))
    }

    /// An `eval`'s string compiled while the preload runs, as
    /// [`Request::preload_file`] compiles a file: with the engine's
    /// strings and map-pointer offsets, optimized and published, so the
    /// classes and functions it declares can be published with the
    /// preload's. Its strings are the engine's from its compile on, an
    /// `eval` whose declarations the preload keeps or not, a growth the
    /// preload bounds; its unit outlives the preload only where the
    /// preload keeps one of its declarations ([`Preloaded::evals`]). A
    /// unit [`Frozen::new`] refuses is served as the request's own, and
    /// what it declares is left out of the preload ([`Request::publish_tables`]).
    pub(crate) fn preload_eval(
        &mut self,
        source: &[u8],
        name: &[u8],
        position: php_compile::syntax::Position,
    ) -> Result<PublishedUnit, crate::unit_cache::CompileRefused> {
        let shared = Arc::clone(&self.shared);
        let kind = php_compile::SourceKind::Eval(position);
        let optimizer = crate::unit_cache::Optimizer::Off;
        let unit = (self.compile_kind_for_cache(source, name, kind, optimizer)).unit;
        if let Ok(unit) = &unit {
            self.count_keys_made(crate::unit_cache::keys_of(unit));
        }
        let optimizer = shared.optimizer();
        let unit = unit.map(|unit| shared.publish_compiled(optimizer.optimized(unit)));
        if let (Ok(Ok(unit)), Some(preloading)) = (&unit, &mut self.preloading) {
            preloading.evals.push(Frozen::clone(unit));
        }
        let unit = unit.map(|unit| unit.map_or_else(|own| own, |unit| PublishedUnit::clone(&unit)));
        unit.map_err(|error| {
            crate::unit_cache::CompileRefused::published(error, |unit| self.take_unit(unit))
        })
    }

    /// Whether the class at `at` of `unit` is one the preload holds,
    /// which a load and a `DECLARE_CLASS` pass over.
    pub(crate) fn preloaded_class(&self, unit: &PublishedUnit, at: usize) -> bool {
        let Ok(at) = u32::try_from(at) else {
            return false;
        };
        self.shared
            .units
            .preloaded()
            .is_some_and(|preloaded| preloaded.holds_class(unit, at))
    }

    /// Whether the function `body` of `unit` is one the preload holds,
    /// which a load and a `DECLARE_FUNCTION` pass over.
    pub(crate) fn preloaded_function(&self, unit: &PublishedUnit, body: Body) -> bool {
        self.shared
            .units
            .preloaded()
            .is_some_and(|preloaded| preloaded.holds_function(unit, body))
    }

    /// `preload_link`: each top-level or anonymous class of a preloaded
    /// file that nothing bound, linked where its name is free and its
    /// parent, interfaces and traits are declared, over and over while
    /// one more links, its link's diagnostics held and a refusal kept as
    /// the class's reason; then php's warning for each one left, in the
    /// order compiled.
    fn preload_link(&mut self) -> Vec<Candidate> {
        let mut candidates = self.candidates();
        loop {
            let mut changed = false;
            for candidate in candidates.iter_mut().filter(|c| !c.linked) {
                if self.link_candidate(candidate) {
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for candidate in candidates.iter().filter(|c| !c.linked) {
            let name = shown(&candidate.decl.name.0).into_owned();
            let (message, file, line) = if !candidate.decl.flags.is_anonymous
                && self.class_named((&candidate.decl.name).into()).is_some()
            {
                (
                    format!("Can't preload already declared class {name}"),
                    candidate.decl.file.0.to_vec(),
                    candidate.decl.line,
                )
            } else if let Some((kind, missing)) = self.missing_dependency(&candidate.decl) {
                (
                    format!("Can't preload unlinked class {name}: {kind}{missing}"),
                    candidate.decl.file.0.to_vec(),
                    candidate.decl.line,
                )
            } else if let Some((reason, file, line)) = &candidate.refused {
                (
                    format!("Can't preload unlinked class {name}: {reason}"),
                    file.clone(),
                    *line,
                )
            } else {
                continue;
            };
            crate::api::raising::raise_at(self, Level::Warning, &message, &file, line);
        }
        candidates
    }

    /// The classes `preload_link` asks about, in the order their files
    /// were compiled: every top-level or anonymous class of a preloaded
    /// file that the request did not bind.
    fn candidates(&self) -> Vec<Candidate> {
        let Some(preloading) = &self.preloading else {
            return Vec::new();
        };
        let mut candidates = Vec::new();
        for (_, unit, _) in &preloading.scripts {
            for (at, decl) in unit.classes.iter().enumerate() {
                let Ok(at) = u32::try_from(at) else {
                    continue;
                };
                let bound = self.own_classes.iter().any(|declared| {
                    PublishedUnit::ptr_eq(&declared.unit, unit) && declared.at == at
                });
                if bound || !(decl.flags.is_top_level || decl.flags.is_anonymous) {
                    continue;
                }
                candidates.push(Candidate {
                    unit: PublishedUnit::clone(unit),
                    at,
                    decl: decl.clone(),
                    linked: false,
                    refused: None,
                });
            }
        }
        candidates
    }

    /// One class linked as `preload_link` links it, its diagnostics
    /// emitted where it links (`zend_emit_recorded_errors`) and its
    /// refusal kept where it does not; whether it linked.
    fn link_candidate(&mut self, candidate: &mut Candidate) -> bool {
        if !candidate.decl.flags.is_anonymous
            && self.class_named((&candidate.decl.name).into()).is_some()
        {
            return false;
        }
        if self.missing_dependency(&candidate.decl).is_some() {
            return false;
        }
        let linking = self.link_against_table(&candidate.decl);
        let refused = match &linking.outcome {
            Linked::Refused(refused) => Some(refused),
            Linked::Linked => linking
                .obligations
                .iter()
                .find_map(|obligation| match obligation {
                    Obligation::Unresolved(refused) => Some(refused),
                    Obligation::Dependency(_) => None,
                }),
        };
        if let Some(refused) = refused {
            candidate.refused = Some((
                refused.refusal.to_string(),
                refused.site.file.0.to_vec(),
                refused.site.line,
            ));
            return false;
        }
        if !linking.obligations.is_empty() {
            return false;
        }
        for raised in &linking.raised {
            crate::api::raising::raise_at(
                self,
                raised.level(),
                &raised.to_string(),
                &raised.site().file.0,
                raised.site().line,
            );
        }
        // The memory limit refusing what the link lays ends the request.
        let bound = self.bind_declared(
            &candidate.unit,
            candidate.at,
            linking.composed,
            linking.record,
        );
        candidate.linked = bound.is_ok();
        candidate.linked
    }

    /// `preload_resolve_deps`: the parent, interface or trait `decl`
    /// names that the table does not hold, what php's warning calls it,
    /// and its name as written; none where every one is declared.
    fn missing_dependency(&self, decl: &ClassDecl) -> Option<(&'static str, String)> {
        let missing = |name: &Name| self.class_named(name.into()).is_none();
        if let Some(parent) = decl.parent.as_ref().filter(|name| missing(name)) {
            return Some(("Unknown parent ", shown(&parent.0).into_owned()));
        }
        if let Some(interface) = decl.interfaces.iter().find(|name| missing(name)) {
            return Some(("Unknown interface ", shown(&interface.0).into_owned()));
        }
        decl.traits
            .iter()
            .find(|name| missing(name))
            .map(|name| ("Unknown trait ", shown(&name.0).into_owned()))
    }

    /// The end of `accel_preload` once its file was served and the request
    /// shut its program down: `preload_link`, then the preload's tables
    /// published in the order php appends them, the scripts made the
    /// cache's saved scripts, and the statistics `opcache_get_status()`
    /// reports. None where nothing is published: no preload, or a class
    /// whose record could not be built, which ends the request with an
    /// internal error.
    pub(crate) fn publish_preload(&mut self) -> Option<Preloaded> {
        let candidates = self.preload_link();
        let preloading = self.preloading.take()?;
        let mut rows = self.class_rows(candidates, preloading.aliases_before);
        self.sort_classes(&mut rows);
        let statistics = self.preload_statistics(&rows, &preloading);
        let mut arms = Vec::new();
        for (_, unit, _) in &preloading.scripts {
            for global in &unit.arms {
                if !arms.contains(global) {
                    arms.push(*global);
                }
            }
        }
        // Only of a unit the preload saves: one it served as its own is
        // dropped with the request, and its address is another's after.
        let mut declared: HashMap<usize, UnitDeclarations> = HashMap::new();
        let saved = |unit: &PublishedUnit| preloading.frozen(unit).is_some();
        for row in self.own_classes.iter().filter(|row| saved(&row.unit)) {
            let held = declared.entry(address(&row.unit)).or_default();
            held.classes.push(row.at);
        }
        for user in
            (self.user_functions.iter()).filter(|user| user.member.is_none() && saved(&user.unit))
        {
            let held = declared.entry(address(&user.unit)).or_default();
            held.functions.push(user.body);
        }
        let tables = match self.publish_tables(&rows, &preloading, &mut declared) {
            Ok(tables) => tables,
            Err(message) => {
                crate::api::raising::fatal(self, &message);
                return None;
            }
        };
        // Only of a unit whose declarations the preload keeps one of: an
        // `eval`'s that keeps none is dropped as the preload ends, and its
        // address is another's after.
        declared.retain(|_, held| held.keeps());
        let evals = (preloading.evals.iter())
            .filter(|unit| declared.contains_key(&address(unit)))
            .cloned()
            .collect();
        let scripts = preloading
            .scripts
            .into_iter()
            .zip(0u64..)
            .map(|((path, unit, size), order)| {
                // `preload_script_in_shared_memory` persists each file.
                let name = self.shared.units.name(&path);
                self.shared.units.persist_names(&unit);
                Arc::new(Held::preloaded(unit, (name, path), size, order))
            })
            .collect();
        Some(Preloaded {
            classes: tables.classes,
            aliases: tables.aliases,
            functions: tables.functions,
            arms,
            scripts,
            evals,
            declared,
            statistics,
        })
    }

    /// The preload request's table of classes in its order, the aliases its
    /// program added where they were added, and the classes that never
    /// linked after the rest.
    fn class_rows(&self, candidates: Vec<Candidate>, aliases_before: usize) -> Vec<Row> {
        let aliases = self.aliases.iter().skip(aliases_before);
        let mut placed: Vec<(Bucket, Row)> = self
            .declared
            .iter()
            .map(|named| (named.place, Row::Class(named.class)))
            .chain(aliases.map(|(place, alias, class)| (*place, Row::Alias(alias.clone(), *class))))
            .collect();
        placed.sort_by_key(|(place, _)| *place);
        let mut rows: Vec<Row> = placed.into_iter().map(|(_, row)| row).collect();
        rows.extend(
            candidates
                .into_iter()
                .filter(|c| !c.linked)
                .map(|c| Row::Unlinked(c.decl.name.0.to_vec())),
        );
        rows
    }

    /// The preload's classes and functions published into the engine's
    /// arena (design §5.14), the preload its first publisher, and the
    /// tables every request starts from built over them in the order
    /// `rows` stand: each class in a bucket of its own and each alias
    /// naming its class's published number, then the functions
    /// reversed. The classes are published each after the classes it
    /// extends, implements and uses ([`Request::publication_order`]),
    /// whatever order `rows` leave them in, so every record is built
    /// from its parent's published one; the request's own rows are
    /// taken out. A class or a function left out is taken out of what
    /// `declared` says the preload holds of its unit. `Err` is the
    /// internal error of a class whose record could not be built, and of
    /// an arena with no room.
    fn publish_tables(
        &mut self,
        rows: &[Row],
        preloading: &Preloading,
        declared: &mut HashMap<usize, UnitDeclarations>,
    ) -> Result<Published, String> {
        let mut renumbered: HashMap<ClassId, EpochClassId> = HashMap::new();
        let mut left = std::collections::HashSet::new();
        for local in self.publication_order(rows) {
            match self.publish_class(local, &renumbered, &left, preloading)? {
                Some(published) => _ = renumbered.insert(local, published),
                None => _ = left.insert(local),
            }
        }
        for row in left.iter().filter_map(|local| self.declared_row(*local)) {
            let held = declared.get_mut(&address(row.unit()));
            if let Some(held) = held {
                held.classes.retain(|at| *at != row.at());
            }
        }
        let shared = |class: ClassId| {
            renumbered
                .get(&class)
                .map_or(class, |&published| published.into())
        };
        self.buckets = Buckets::default();
        let mut classes = Vec::new();
        let mut aliases = Vec::new();
        for row in rows {
            match row {
                Row::Class(local) if !left.contains(local) => {
                    let Some(decl) = self.symbols().class_decl(*local) else {
                        continue;
                    };
                    let key = Box::from(&*decl.name.0);
                    let place = self.buckets.take();
                    let class = shared(*local);
                    classes.push((key, Named { place, class }));
                }
                Row::Alias(alias, class) if !left.contains(class) => {
                    let place = self.buckets.take();
                    let key = alias.clone().into_boxed_slice();
                    aliases.push((key, (place, alias.clone(), shared(*class))));
                }
                Row::Class(_) | Row::Alias(..) | Row::Unlinked(_) => {}
            }
        }
        self.closure_rows.clear();
        // A function of a unit the preload holds as its own, one
        // `Frozen::new` refused, is left out of it, and so is one whose
        // row the arena refuses, which a load of its unit then declares.
        let mut functions: Vec<(Box<[u8]>, UserFunction<InArena>)> = Vec::new();
        for user in std::mem::take(&mut self.user_functions) {
            let Some(decl) = (user.member.is_none())
                .then(|| user_function_decl(super::FunctionRow::Own(&user)))
                .flatten()
            else {
                continue;
            };
            let name = Box::<[u8]>::from(&*decl.name.0);
            let Some(frozen) = preloading.frozen(&user.unit).cloned() else {
                continue;
            };
            if name.starts_with(b"{") {
                continue;
            }
            let (unit, body) = (address(&user.unit), user.body);
            match user.published(frozen, crate::readable::NamedIn::Unit) {
                Some(user) => functions.push((name, user)),
                None => {
                    if let Some(held) = declared.get_mut(&unit) {
                        held.functions.retain(|held| *held != body);
                    }
                }
            }
        }
        functions.reverse();
        let count = u32::try_from(functions.len()).map_err(|_| ARENA_FULL.to_string())?;
        let reserved = self
            .arena
            .reserve_functions(count)
            .ok_or_else(|| ARENA_FULL.to_string())?;
        let mut published = Vec::with_capacity(functions.len());
        for (function, (name, user)) in reserved.functions().zip(functions) {
            if !self.arena.publish_function(function, user) {
                return Err(ARENA_FULL.to_string());
            }
            published.push((name, Function::from(function)));
        }
        self.functions.clear_own();
        self.declared.clear_own();
        // The classes found so far are numbered as the request's own,
        // which the preload's publication numbers again.
        self.ce_caches.take();
        self.class_names = super::ClassNames::default();
        self.aliases.clear_own();
        self.own_classes.clear();
        self.class_natives.clear();
        Ok(Published {
            classes: Rows::of(classes),
            aliases: Rows::of(aliases),
            functions: Rows::of(published),
        })
    }

    /// `zend_shared_alloc_save_state` after preloading: the arena the
    /// preload published into laid below every epoch from now on, so a
    /// restart keeps its rows at their numbers, with the units it saved,
    /// its files' and the `eval`s' it keeps a declaration of
    /// ([`Preloaded::evals`]), whose words every epoch lays its own of,
    /// counting where the tier counts a unit the cache holds now, as
    /// php's `zend_jit_script` lays the words of `$PRELOAD$`'s op arrays
    /// ([`crate::arena::Epochs::lay_base`]).
    pub(crate) fn save_preload_state(&self) {
        let units = (self.shared.units.preloaded())
            .map(|preloaded| {
                let scripts = preloaded.scripts.iter().map(|held| &held.unit);
                scripts.chain(&preloaded.evals)
            })
            .into_iter()
            .flatten()
            .map(Frozen::clone)
            .collect();
        let laid = crate::tier::words_laid(&self.shared, Some(self));
        if laid.counts() {
            self.published_counting();
        }
        self.shared.arena.lay_base(&self.arena, units, laid);
    }

    /// The order the preload's classes are published in: each in the
    /// order `rows` stand, after every class of the request's own it
    /// names as its parent, an interface or a trait, so no record is
    /// built before the records it copies. A class that named itself
    /// through its chain would come before what it names; no linked
    /// class can.
    fn publication_order(&self, rows: &[Row]) -> Vec<ClassId> {
        let mut order = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let Row::Class(first) = row else {
                continue;
            };
            let mut stack = vec![(*first, false)];
            while let Some((class, named)) = stack.pop() {
                if named {
                    order.push(class);
                    continue;
                }
                if class.is_published() || !seen.insert(class) {
                    continue;
                }
                stack.push((class, true));
                let Some(decl) = self.symbols().class_decl(class) else {
                    continue;
                };
                let depends = (decl.parent.iter())
                    .chain(&decl.interfaces)
                    .chain(&decl.traits)
                    .filter_map(|name| self.class_named(name.into()));
                for depend in depends.collect::<Vec<_>>().into_iter().rev() {
                    stack.push((depend, false));
                }
            }
        }
        order
    }

    /// The request's own class `local` published ([`Request::publish`]):
    /// its record built over the published classes `renumbered` names,
    /// whose every class it extends, implements or uses is one already,
    /// or one the arena's one check finds every request shares.
    fn publish_class(
        &mut self,
        local: ClassId,
        renumbered: &HashMap<ClassId, EpochClassId>,
        left: &std::collections::HashSet<ClassId>,
        preloading: &Preloading,
    ) -> Result<Option<EpochClassId>, String> {
        let name = String::from_utf8_lossy(self.symbols().class_name(local)).into_owned();
        let shared = |class: ClassId| match renumbered.get(&class) {
            Some(published) => Some(*published),
            None => self.arena.shared_class(class),
        };
        let Some(row) = self.declared_row(local) else {
            return Err(format!(
                "InternalError: the preload's class {name} has no row"
            ));
        };
        // Declared by a unit the preload holds as its own, or naming a
        // class so declared, the class is left out of the preload, as a
        // class of no preloaded file is.
        let Some(frozen) = preloading.frozen(row.unit()).cloned() else {
            return Ok(None);
        };
        let names_left = row.decl().is_some_and(|decl| {
            (decl.parent.iter())
                .chain(&decl.interfaces)
                .chain(&decl.traits)
                .filter_map(|name| self.class_named(name.into()))
                .any(|class| left.contains(&class))
        });
        if names_left {
            return Ok(None);
        }
        let named =
            |name: &[u8]| shared(self.class_named(Bare::resolved(name))?).map(ClassId::from);
        let Some(record) = self.record_over(row, &named) else {
            return Err(format!(
                "InternalError: the preload's class {name} has no record: a class it names was not published before it"
            ));
        };
        let parent = row
            .decl()
            .and_then(|decl| self.class_named(decl.parent.as_ref()?.into()))
            .and_then(shared);
        let composed = row.composed().map(|composed| Composed {
            decl: composed.decl.clone(),
            bodies: composed.bodies.to_vec(),
            abstracts: composed.abstracts,
        });
        let units = |unit: &PublishedUnit| preloading.frozen(unit).cloned();
        match self.publish((&frozen, row.at()), composed, &record, parent, &units) {
            Ok(class) => Ok(Some(class)),
            // The arena refusing the class leaves it out of the preload,
            // and out of what the preload holds of its unit, so a request
            // that loads the unit binds it as its own; a request that
            // does not finds no class of the name (#4699).
            Err(Unpublished::Refused) => Ok(None),
            Err(Unpublished::Full) => Err(ARENA_FULL.to_string()),
        }
    }

    /// `preload_sort_classes`, php's own swap sort and not a stable one:
    /// a linked class whose parent, or one of whose interfaces, is a
    /// class of the program standing later swaps places with that row,
    /// an alias of it included, and the row it moved to is looked at
    /// again.
    fn sort_classes(&self, rows: &mut [Row]) {
        let named = |row: &Row| match row {
            Row::Class(class) | Row::Alias(_, class) => Some(*class),
            Row::Unlinked(_) => None,
        };
        let mut at = 0;
        let mut guard = rows.len().saturating_mul(rows.len()).saturating_add(1);
        while at < rows.len() && guard > 0 {
            guard -= 1;
            let depends: Vec<ClassId> = match rows.get(at) {
                Some(Row::Class(class) | Row::Alias(_, class)) => self
                    .symbols()
                    .class_decl(*class)
                    .map(|decl| {
                        decl.parent
                            .iter()
                            .chain(decl.interfaces.iter())
                            .filter_map(|name| self.class_named(name.into()))
                            .filter(|class| !class.is_published())
                            .collect()
                    })
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            let later = depends.iter().find_map(|wanted| {
                rows.iter()
                    .skip(at + 1)
                    .position(|row| named(row) == Some(*wanted))
                    .map(|found| at + 1 + found)
            });
            match later {
                Some(later) => rows.swap(at, later),
                None => at += 1,
            }
        }
    }

    /// `opcache_get_status()['preload_statistics']` as the preload's
    /// tables will stand: the weight, each function by name in the
    /// order the table holds them, each row of the table of classes, a class
    /// by its name and an alias by its key, and the scripts.
    fn preload_statistics(&self, rows: &[Row], preloading: &Preloading) -> PreloadStatistics {
        let functions = self
            .user_functions
            .iter()
            .filter(|user| user.member.is_none())
            .filter_map(|user| user_function_decl(super::FunctionRow::Own(user)))
            .filter(|decl| !decl.name.0.starts_with(b"{"))
            .map(|decl| decl.name.0.to_vec())
            .rev()
            .collect();
        let classes = rows
            .iter()
            .map(|row| match row {
                Row::Class(class) => self
                    .symbols()
                    .class_decl(*class)
                    .map(|decl| decl.name.0.to_vec())
                    .unwrap_or_default(),
                Row::Alias(alias, _) => alias.clone(),
                Row::Unlinked(name) => name.clone(),
            })
            .collect();
        PreloadStatistics {
            memory_consumption: preloading
                .scripts
                .iter()
                .map(|(_, _, size)| *size)
                .fold(0, u64::saturating_add),
            functions,
            classes,
            scripts: preloading
                .scripts
                .iter()
                .map(|(path, _, _)| path.clone())
                .collect(),
        }
    }
}

impl Code<&Request> {
    /// `CG(compiler_options) & ZEND_COMPILE_PRELOAD`: whether this is the
    /// request `opcache.preload`'s file is served in, whose declarations
    /// every request after starts from, as `FFI::load` registers the
    /// scope a header names for `FFI::scope` in it (`zend_ffi_load`).
    #[must_use]
    pub fn preloading(self) -> bool {
        self.0.preloading.is_some()
    }

    /// `CG(compiler_options) & ZEND_COMPILE_PRELOAD_IN_CHILD`: whether
    /// this is the preload's request served on a thread of its own
    /// ([`crate::api::abi::PreloadThread`]), php's preload child, as
    /// opcache serves it under `opcache.preload_user`, where `FFI::load`
    /// refuses to load a header (`Z/ext/ffi/ffi.c:3628`).
    #[must_use]
    pub fn preloading_in_child(self) -> bool {
        self.0
            .preloading
            .as_ref()
            .is_some_and(|preloading| preloading.in_child)
    }
}

impl Frames<&Request> {
    /// `EX(prev_execute_data)->func->common.fn_flags &
    /// ZEND_ACC_PRELOADED`: whether the native being served was called
    /// from a function, a method or a closure the preload compiled, as
    /// `zend_compile_func_decl` flags each one under
    /// `ZEND_COMPILE_PRELOAD`; a file's main code is no function and is
    /// never one. What `FFI`'s API restriction under
    /// `ffi.enable=preload` admits in any host (`zend_ffi_validate_api_restriction`).
    #[must_use]
    pub fn caller_preloaded(self) -> bool {
        let Some(preloaded) = self.0.shared.units.preloaded() else {
            return false;
        };
        let mut chain = self.0.chain();
        if !matches!(chain.next(), Some(super::Link::Call(_))) {
            return false;
        }
        let Some(super::Link::Frame(_, state)) = chain.next() else {
            return false;
        };
        let Some((unit, body)) = state.unit() else {
            return false;
        };
        body != Body::Main
            && (preloaded.scripts.iter()).any(|held| PublishedUnit::ptr_eq(&held.unit, unit))
    }
}

/// What the arena could not take: more classes or functions published
/// than a number can name.
const ARENA_FULL: &str = "InternalError: the engine's published arena is full";

/// The preload's tables as [`Request::publish_tables`] left them.
struct Published {
    classes: Rows<Named>,
    aliases: Rows<Alias>,
    functions: Rows<Function>,
}

/// The tables a request starts from: the preload's rows where the
/// engine preloaded, and none where it did not.
pub(crate) struct Tables {
    pub(crate) classes: Keyed<Named>,
    pub(crate) aliases: Keyed<Alias>,
    pub(crate) functions: Keyed<Function>,
}

impl Tables {
    pub(crate) fn of(preloaded: Option<&Preloaded>) -> Tables {
        match preloaded {
            Some(preloaded) => Tables {
                classes: Keyed::over(&preloaded.classes),
                aliases: Keyed::over(&preloaded.aliases),
                functions: Keyed::over(&preloaded.functions),
            },
            None => Tables {
                classes: Keyed::default(),
                aliases: Keyed::default(),
                functions: Keyed::default(),
            },
        }
    }
}

impl Request {
    /// The preload's file opened nothing: `php_stream_open_for_zend`'s
    /// warning with no function to name, the `strerror` of the plain
    /// files wrapper's `open` of the path as written, which php tries
    /// once `zend_resolve_path` found nothing (a file the preload's user
    /// may not reach is "Permission denied"), and the `require`'s fatal,
    /// at no file and no line, as php's startup raises them.
    pub(crate) fn preload_unopened(&mut self, script: &[u8]) {
        let include_path = String::from_utf8_lossy(self.include_path()).into_owned();
        let path = self
            .environment()
            .cwd()
            .join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(script));
        let reason = std::fs::File::open(path)
            .err()
            .and_then(|error| error.raw_os_error())
            .map_or_else(
                || String::from("No such file or directory"),
                |code| self.environment().os_error_text(code),
            );
        crate::api::raising::raise_at(
            self,
            Level::Warning,
            &format!("PHP Startup: Failed to open stream: {reason}"),
            UNKNOWN,
            0,
        );
        let message = format!(
            "Failed opening required '{}' (include_path='{include_path}')",
            String::from_utf8_lossy(script)
        );
        self.end_in_compile_time(Level::CompileError, &message, UNKNOWN, 0);
    }
}

/// What `php_error_cb` names for no file, `error_filename` unset.
const UNKNOWN: &[u8] = b"Unknown";

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;

    use super::{Request, Row};
    use crate::api::request::PropertyInfo;
    use crate::api::values::ClassId;
    use crate::engine::{Engine, RequestDescription};

    /// How many rounds each side runs.
    const ROUNDS: usize = 40;

    /// The name `class` is declared under, as the rounds hand it on.
    fn name_of(req: &Request, class: ClassId) -> String {
        String::from_utf8_lossy(req.symbols().class_name(class)).into_owned()
    }

    /// One thread publishes into the engine's arena as the preload does,
    /// round after round, while a request on another declares classes of
    /// its own and reads each class the first published as it is handed
    /// its number (design §5.14, *Identities*). Neither side's numbers
    /// move for the other's: the request's classes keep the numbers they
    /// were declared under and stay its own, and every published number
    /// names the class it was published as, read by the request with nothing
    /// taken however far the arena grew after it.
    #[test]
    fn a_publisher_and_a_request_on_two_threads_never_move_each_others_numbers() {
        let engine = Engine::builder().build();
        let (handed, taken) = mpsc::channel::<(ClassId, String)>();
        std::thread::scope(|scope| {
            let shared = Arc::clone(&engine.shared);
            let publisher = scope.spawn(move || {
                let mut published = Vec::new();
                for round in 0..ROUNDS {
                    let mut req = Request::new(Arc::clone(&shared), RequestDescription::default());
                    req.begin_preload(false);
                    let code = format!(
                        "class P{round} {{ function m() {{ return {round}; }} }} class Q{round} extends P{round} {{}}"
                    );
                    assert!(req.code_mut().eval_stringl(code.as_bytes(), b"publisher").is_ok());
                    let Some(preloaded) = req.publish_preload() else {
                        return Err(format!("round {round} published nothing"));
                    };
                    for named in preloaded.classes.rows.iter() {
                        let name = name_of(&req, named.class);
                        assert!(named.class.is_published(), "{name} is published");
                        published.push((named.class, name.clone()));
                        if handed.send((named.class, name)).is_err() {
                            return Err(String::from("the request stopped listening"));
                        }
                    }
                    // Every number published so far still names its
                    // class, however many the request declared meanwhile.
                    for (class, name) in &published {
                        assert_eq!(name_of(&req, *class), *name, "{class:?} moved");
                    }
                }
                Ok(published)
            });
            let for_request = Arc::clone(&engine.shared);
            let request = scope.spawn(move || {
                let mut req = Request::new(for_request, RequestDescription::default());
                let mut own: Vec<(ClassId, String)> = Vec::new();
                let mut seen: Vec<(ClassId, String)> = Vec::new();
                for round in 0..ROUNDS {
                    let name = format!("R{round}");
                    let code = format!("class {name} {{ function m() {{ return {round}; }} }}");
                    assert!(
                        req.code_mut()
                            .eval_stringl(code.as_bytes(), b"request")
                            .is_ok()
                    );
                    let Some(class) =
                        req.class_named(php_unit::decl::Bare::resolved(name.as_bytes()))
                    else {
                        return Err(format!("{name} was not declared"));
                    };
                    assert!(!class.is_published(), "{name} is the request's own");
                    assert_eq!(
                        Some(class),
                        ClassId::local(round),
                        "{name} took the next row"
                    );
                    own.push((class, name));
                    // A round's two classes, waited for, so each round of
                    // the request's reads what the publisher published
                    // while it declared, as the publisher goes on.
                    for _ in 0..2 {
                        let Ok(handed) = taken.recv() else {
                            return Err(String::from("the publisher stopped"));
                        };
                        seen.push(handed);
                    }
                    for (class, name) in own.iter().chain(&seen) {
                        assert_eq!(name_of(&req, *class), *name, "{class:?} moved");
                    }
                }
                seen.extend(taken.iter());
                for (class, name) in own.iter().chain(&seen) {
                    assert_eq!(name_of(&req, *class), *name, "{class:?} moved");
                }
                let child = seen
                    .iter()
                    .find(|(_, name)| name == "Q0")
                    .map(|(class, _)| *class);
                let inherited = child.and_then(|child| req.symbols().method_named(child, b"m"));
                // Shared by every request: not one of this request's own rows.
                let shared = |function: crate::api::call::Function| {
                    function.0 & crate::api::call::Function::LOCAL == 0
                };
                assert!(inherited.is_some_and(shared), "Q0 inherits P0's m");
                Ok(seen.len())
            });
            let numbers = publisher
                .join()
                .unwrap_or_else(|_| Err(String::from("panicked")));
            let seen = request
                .join()
                .unwrap_or_else(|_| Err(String::from("panicked")));
            assert_eq!(seen, Ok(2 * ROUNDS));
            let reader = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
            assert_eq!(numbers.as_ref().map(Vec::len), Ok(2 * ROUNDS));
            for (class, name) in numbers.expect("the publisher's numbers") {
                assert_eq!(name_of(&reader, class), name, "{class:?} moved");
            }
        });
    }

    /// The preload's table with a class before its parent, which it
    /// names through an alias, and before the trait it uses: the order
    /// `preload_sort_classes` would leave where its guard ran out. Every
    /// class is published after what it extends and uses all the same,
    /// so its record is built from its parent's and holds the inherited
    /// and the trait's methods, the alias names the parent's published
    /// number, and the table keeps the order it was handed.
    #[test]
    fn a_class_before_its_parent_in_the_table_is_published_with_its_record() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        req.begin_preload(false);
        let code = "trait T { public $t; function t() {} } \
                    class P { public $p; private $q; function p() {} } \
                    class_alias('P', 'PA'); class C extends PA { use T; public $c; public $p; }";
        assert!(
            req.code_mut()
                .eval_stringl(code.as_bytes(), b"preload")
                .is_ok()
        );
        let mut rows = req.class_rows(Vec::new(), 0);
        let child = req.class_named(php_unit::decl::Bare::resolved(b"c"));
        let at = rows
            .iter()
            .position(|row| matches!(row, Row::Class(class) if Some(*class) == child));
        if let Some(at) = at {
            rows[..=at].rotate_right(1);
        }
        let preloading = req.preloading.take().expect("the preload's request");
        let published =
            req.publish_tables(&rows, &preloading, &mut std::collections::HashMap::new());
        let names: Vec<(ClassId, String)> = published
            .iter()
            .flat_map(|published| published.classes.rows.iter())
            .map(|named| (named.class, name_of(&req, named.class)))
            .collect();
        let order: Vec<&str> = names.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(order, ["C", "T", "P"]);
        let child = names.first().map(|(class, _)| *class);
        assert!(child.is_some_and(ClassId::is_published));
        // The placed properties came with the published row: the
        // parent's slots first, the redeclared `$p` in the parent's,
        // naming the published classes.
        let placed = |class: ClassId| -> Vec<(ClassId, String)> {
            req.arena.class(class).map_or_else(Vec::new, |row| {
                let shown = |info: PropertyInfo| {
                    let name = req.property_decl(info).map_or(&[][..], |p| &p.name.0);
                    (info.declaring, String::from_utf8_lossy(name).into_owned())
                };
                let row = crate::api::request::ClassRow::Arena(row);
                row.placed().slots().iter().map(shown).collect()
            })
        };
        let parent = names.last().map(|(class, _)| *class);
        if let (Some(child), Some(parent)) = (child, parent) {
            let expect = |pairs: &[(ClassId, &str)]| -> Vec<(ClassId, String)> {
                pairs.iter().map(|(c, n)| (*c, (*n).to_string())).collect()
            };
            assert_eq!(placed(parent), expect(&[(parent, "p"), (parent, "q")]));
            let own = [(child, "p"), (parent, "q"), (child, "c"), (child, "t")];
            assert_eq!(placed(child), expect(&own));
        }
        for method in [&b"t"[..], b"p"] {
            let found = child.and_then(|child| req.symbols().method_named(child, method));
            assert!(found.is_some_and(|found| req.user_function(found).is_some()));
        }
        let alias = published
            .iter()
            .flat_map(|published| published.aliases.rows.iter())
            .map(|(_, alias, class)| (alias.clone(), *class))
            .next();
        assert_eq!(alias, parent.map(|parent| (b"pa".to_vec(), parent)));
    }
}

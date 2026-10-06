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

use std::collections::HashMap;
use std::sync::Arc;

use php_unit::decl::{ClassDecl, Name, shown};
use php_unit::link::{Linked, Obligation};
use php_unit::{AutoGlobal, Level, Unit};

use super::{Bound, Declared, PreloadStatistics, Request, UserFunction, user_function_decl};
use crate::api::values::ClassId;
use crate::engine::Cached;
use crate::enums::EnumMethod;
use crate::exec::Body;
use crate::unit_cache::{Held, full_path};

/// One of a request's tables that starts from the preload's rows and
/// grows past them: php's `EG(class_table)` and `EG(function_table)`,
/// which `preload_load` extended once, at startup, so every request
/// finds the preloaded classes and functions before its first line.
/// The preload's rows are the engine's, shared; the request's own
/// follow them and are dropped with it.
pub(crate) struct Stacked<T> {
    preloaded: Arc<[T]>,
    own: Vec<T>,
}

impl<T> Stacked<T> {
    /// A table whose first rows are `preloaded`.
    pub(crate) fn over(preloaded: Arc<[T]>) -> Stacked<T> {
        Stacked {
            preloaded,
            own: Vec::new(),
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

    /// The row at `at` to change, where it is the request's own; none
    /// for the preload's, which are published and shared.
    fn own_mut(&mut self, at: usize) -> Option<&mut T> {
        self.own.get_mut(at.checked_sub(self.preloaded.len())?)
    }

    pub(crate) fn push(&mut self, row: T) {
        self.own.push(row);
    }

    pub(crate) fn extend(&mut self, rows: impl IntoIterator<Item = T>) {
        self.own.extend(rows);
    }

    /// The request's own rows, taken out.
    fn take_own(&mut self) -> Vec<T> {
        std::mem::take(&mut self.own)
    }
}

impl<T> Default for Stacked<T> {
    fn default() -> Stacked<T> {
        Stacked::over(Arc::new([]))
    }
}

/// A row's key in `EG(class_table)`: the lowered name `zend_hash_add`
/// files it under, none for a row no name finds.
pub(crate) trait Key {
    fn key(&self) -> Option<Box<[u8]>>;
}

/// Where each key of a table stands: the row, counted as
/// [`Stacked::get`] counts it.
type Keys = HashMap<Box<[u8]>, usize>;

/// The keys of `rows`, the first row of a key kept, as `zend_hash_add`
/// refuses a key the table holds.
fn keys_of<T: Key>(rows: &[T], from: usize) -> Keys {
    let mut keys = Keys::with_capacity(rows.len());
    for (at, row) in (from..).zip(rows) {
        if let Some(key) = row.key() {
            keys.entry(key).or_insert(at);
        }
    }
    keys
}

/// The preload's rows of a keyed table with their keys, shared by every
/// request as the rows are.
pub(crate) struct Rows<T> {
    rows: Arc<[T]>,
    keys: Arc<Keys>,
}

impl<T: Key> Rows<T> {
    fn of(rows: Vec<T>) -> Rows<T> {
        let keys = Arc::new(keys_of(&rows, 0));
        Rows {
            rows: rows.into(),
            keys,
        }
    }
}

/// A [`Stacked`] table of `EG(class_table)` found by key in one hash lookup,
/// as `zend_hash_find` finds a bucket, as well as by row: each row
/// under its [`Key`], the preload's keys shared with the preload's rows
/// and the request's own dropped with its own.
pub(crate) struct Keyed<T> {
    rows: Stacked<T>,
    preloaded: Arc<Keys>,
    own: Keys,
}

impl<T: Key> Keyed<T> {
    /// A table whose first rows are the preload's.
    fn over(preloaded: &Rows<T>) -> Keyed<T> {
        Keyed {
            rows: Stacked::over(Arc::clone(&preloaded.rows)),
            preloaded: Arc::clone(&preloaded.keys),
            own: Keys::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        self.rows.get(at)
    }

    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.rows.iter()
    }

    /// The row at `at` to change, where it is the request's own
    /// ([`Stacked::own_mut`]); what changes must leave its key as it is.
    pub(crate) fn own_mut(&mut self, at: usize) -> Option<&mut T> {
        self.rows.own_mut(at)
    }

    /// The row filed under the lowered `key`, and where it stands.
    pub(crate) fn find(&self, key: &[u8]) -> Option<(usize, &T)> {
        let at = *self.preloaded.get(key).or_else(|| self.own.get(key))?;
        Some((at, self.rows.get(at)?))
    }

    pub(crate) fn push(&mut self, row: T) {
        if let Some(key) = row.key() {
            self.own.entry(key).or_insert(self.rows.len());
        }
        self.rows.push(row);
    }

    /// The request's own rows, taken out with their keys.
    fn take_own(&mut self) -> Vec<T> {
        self.own.clear();
        self.rows.take_own()
    }
}

impl<T: Key> Default for Keyed<T> {
    fn default() -> Keyed<T> {
        Keyed::over(&Rows::of(Vec::new()))
    }
}

impl Key for Alias {
    fn key(&self) -> Option<Box<[u8]>> {
        Some(self.1.clone().into_boxed_slice())
    }
}

impl Key for Declared {
    fn key(&self) -> Option<Box<[u8]>> {
        Some(self.decl()?.name.0.to_ascii_lowercase().into_boxed_slice())
    }
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
    unit: Arc<Unit>,
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
    pub(crate) fn reserve(&mut self, unit: &Arc<Unit>, at: u32) {
        let bucket = self.take();
        let unit = Arc::clone(unit);
        self.reserved.push(Reserved { unit, at, bucket });
    }

    /// The bucket held for the class at `at` of `unit`, or the next.
    pub(crate) fn claim(&mut self, unit: &Arc<Unit>, at: u32) -> Bucket {
        let held = (self.reserved.iter()).position(|r| Arc::ptr_eq(&r.unit, unit) && r.at == at);
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
    pub(crate) classes: Rows<Declared>,
    pub(crate) aliases: Rows<Alias>,
    pub(crate) functions: Arc<[UserFunction]>,
    pub(crate) enum_functions: Arc<[(ClassId, EnumMethod)]>,
    /// `ping_auto_globals_mask`: the superglobals the preloaded code
    /// names, armed at every request's start (`preload_activate`).
    pub(crate) arms: Vec<AutoGlobal>,
    /// Each file the preload compiled, in the order compiled: php's
    /// saved scripts, held in the cache, never validated and outliving
    /// every restart.
    pub(crate) scripts: Vec<Arc<Held>>,
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

impl Preloaded {
    /// Whether the class at `at` of `unit` is one the preload holds.
    pub(crate) fn holds_class(&self, unit: &Arc<Unit>, at: u32) -> bool {
        self.declared
            .get(&address(unit))
            .is_some_and(|held| held.classes.contains(&at))
    }

    /// Whether the function `body` of `unit` is one the preload holds.
    pub(crate) fn holds_function(&self, unit: &Arc<Unit>, body: Body) -> bool {
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
fn address(unit: &Arc<Unit>) -> usize {
    Arc::as_ptr(unit) as usize
}

/// The preload's request: the files it compiled, in order, and how
/// many aliases the extensions' `RINIT`s had added before its program
/// began, which are theirs and every request's, not the preload's.
pub(crate) struct Preloading {
    scripts: Vec<(Vec<u8>, Arc<Unit>, u64)>,
    aliases_before: usize,
}

/// One class `preload_link` asks about: a top-level or an anonymous
/// class of a preloaded file that nothing bound.
struct Candidate {
    unit: Arc<Unit>,
    at: u32,
    decl: ClassDecl,
    linked: bool,
    /// The last refusal its link met, with the site php names.
    refused: Option<(String, Vec<u8>, u32)>,
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
    /// `CG(compiler_options) & ZEND_COMPILE_PRELOAD`: whether this is the
    /// request `opcache.preload`'s file is served in, whose declarations
    /// every request after starts from, as `FFI::load` registers the
    /// scope a header names for `FFI::scope` in it (`zend_ffi_load`).
    #[must_use]
    pub fn preloading(&self) -> bool {
        self.preloading.is_some()
    }

    /// `EX(prev_execute_data)->func->common.fn_flags &
    /// ZEND_ACC_PRELOADED`: whether the native being served was called
    /// from a function, a method or a closure the preload compiled, as
    /// `zend_compile_func_decl` flags each one under
    /// `ZEND_COMPILE_PRELOAD`; a file's main code is no function and is
    /// never one. What `FFI`'s API restriction under
    /// `ffi.enable=preload` admits in any host (`zend_ffi_validate_api_restriction`).
    #[must_use]
    pub fn caller_preloaded(&self) -> bool {
        let Some(preloaded) = self.shared.units.preloaded() else {
            return false;
        };
        let mut chain = self.chain();
        if !matches!(chain.next(), Some(super::Link::Call(_))) {
            return false;
        }
        let Some(super::Link::Frame(_, state)) = chain.next() else {
            return false;
        };
        state.body != Body::Main
            && preloaded.scripts.iter().any(|held| {
                held.unit
                    .as_ref()
                    .is_ok_and(|unit| Arc::ptr_eq(unit, &state.unit))
            })
    }

    /// The request, started, is the preload's from here: every file it
    /// compiles is compiled into the preload.
    pub(crate) fn begin_preload(&mut self) {
        self.preloading = Some(Preloading {
            scripts: Vec::new(),
            aliases_before: self.aliases.len(),
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
            return Some(Ok(Arc::clone(unit)));
        }
        let source = std::fs::read(String::from_utf8_lossy(path).as_ref()).ok()?;
        let shared = Arc::clone(&self.shared);
        let bound = crate::multibyte::bound(self, shared.compile_multibyte());
        let holds = |name: &[u8]| self.class_named(name).is_some();
        let request_classes = php_compile::RequestClasses::new(&holds);
        let unit = shared.publish_file(
            &source,
            path,
            &self.cwd,
            self.stack_limit(),
            request_classes,
            bound.as_ref().map(crate::multibyte::Bound::settings),
        );
        if let (Ok(unit), Some(preloading)) = (&unit, &mut self.preloading) {
            let size = u64::try_from(source.len()).unwrap_or(u64::MAX);
            preloading.scripts.push((full, Arc::clone(unit), size));
        }
        Some(unit)
    }

    /// Whether the class at `at` of `unit` is one the preload holds,
    /// which a load and a `DECLARE_CLASS` pass over.
    pub(crate) fn preloaded_class(&self, unit: &Arc<Unit>, at: usize) -> bool {
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
    pub(crate) fn preloaded_function(&self, unit: &Arc<Unit>, body: Body) -> bool {
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
                && self.class_named(&candidate.decl.name.0).is_some()
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
                let bound = self
                    .declared
                    .iter()
                    .any(|declared| Arc::ptr_eq(&declared.unit, unit) && declared.at == at);
                if bound || !(decl.flags.is_top_level || decl.flags.is_anonymous) {
                    continue;
                }
                candidates.push(Candidate {
                    unit: Arc::clone(unit),
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
        if !candidate.decl.flags.is_anonymous && self.class_named(&candidate.decl.name.0).is_some()
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
        self.bind_declared(
            &candidate.unit,
            candidate.at,
            linking.composed,
            linking.record,
        );
        candidate.linked = true;
        true
    }

    /// `preload_resolve_deps`: the parent, interface or trait `decl`
    /// names that the table does not hold, what php's warning calls it,
    /// and its name as written; none where every one is declared.
    fn missing_dependency(&self, decl: &ClassDecl) -> Option<(&'static str, String)> {
        let missing = |name: &Name| self.class_named(&name.0).is_none();
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
    /// taken out of the request in the order php appends them, the
    /// scripts made the cache's saved scripts, and the statistics
    /// `opcache_get_status()` reports.
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
        self.rebuild_tables(&rows);
        let mut declared: HashMap<usize, UnitDeclarations> = HashMap::new();
        for row in self.declared.iter() {
            let held = declared.entry(address(&row.unit)).or_default();
            held.classes.push(row.at);
        }
        for user in self
            .user_functions
            .iter()
            .filter(|user| user.member.is_none())
        {
            let held = declared.entry(address(&user.unit)).or_default();
            held.functions.push(user.body);
        }
        let scripts = preloading
            .scripts
            .into_iter()
            .zip(0u64..)
            .map(|((path, unit, size), order)| Arc::new(Held::preloaded(unit, path, size, order)))
            .collect();
        Some(Preloaded {
            classes: Rows::of(self.declared.take_own()),
            aliases: Rows::of(self.aliases.take_own()),
            functions: self.user_functions.take_own().into(),
            enum_functions: self.enum_functions.take_own().into(),
            arms,
            scripts,
            declared,
            statistics,
        })
    }

    /// The preload request's table of classes in its order, the aliases its
    /// program added where they were added, and the classes that never
    /// linked after the rest.
    fn class_rows(&mut self, candidates: Vec<Candidate>, aliases_before: usize) -> Vec<Row> {
        let natives = self.shared.classes.len();
        let aliases = self.aliases.take_own().into_iter().skip(aliases_before);
        let mut placed: Vec<(Bucket, Row)> = (natives..)
            .zip(self.declared.iter())
            .map(|(at, row)| (row.place, Row::Class(class_id(at))))
            .chain(aliases.map(|(place, alias, class)| (place, Row::Alias(alias, class))))
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

    /// The tables again from nothing, in the order `rows` stand: each
    /// class a new number and each alias the number of its class, the
    /// functions reversed, then each class's members numbered past them.
    fn rebuild_tables(&mut self, rows: &[Row]) {
        let natives = self.shared.classes.len();
        let mut slots: Vec<Option<Declared>> =
            self.declared.take_own().into_iter().map(Some).collect();
        let functions: Vec<UserFunction> = self
            .user_functions
            .take_own()
            .into_iter()
            .filter(|user| {
                user.member.is_none()
                    && user_function_decl(user).is_some_and(|decl| !decl.name.0.starts_with(b"{"))
            })
            .rev()
            .collect();
        let _ = self.enum_functions.take_own();
        self.buckets = Buckets::default();
        let mut renumbered: Vec<(ClassId, ClassId)> = Vec::new();
        for row in rows {
            match row {
                Row::Class(old) => {
                    let Some(declared) = usize::try_from(old.0)
                        .ok()
                        .and_then(|at| at.checked_sub(natives))
                        .and_then(|at| slots.get_mut(at))
                        .and_then(Option::take)
                    else {
                        continue;
                    };
                    renumbered.push((*old, class_id(natives + self.declared.len())));
                    let place = self.buckets.take();
                    // Renumbered, so the placed rows are built again
                    // under the new numbers, and the members bound again
                    // below.
                    self.declared.push(Declared {
                        place,
                        placed: std::sync::OnceLock::new(),
                        bound: Bound::default(),
                        ..declared
                    });
                }
                Row::Alias(alias, old) => {
                    let class = renumbered
                        .iter()
                        .find(|(from, _)| from == old)
                        .map_or(*old, |(_, to)| *to);
                    let place = self.buckets.take();
                    self.aliases.push((place, alias.clone(), class));
                }
                Row::Unlinked(_) => {}
            }
        }
        self.user_functions.extend(functions);
        for (_, class) in renumbered {
            self.bind_members(class, None);
        }
    }

    /// `preload_sort_classes`, php's own swap sort and not a stable one:
    /// a linked class whose parent, or one of whose interfaces, is a
    /// class of the program standing later swaps places with that row,
    /// an alias of it included, and the row it moved to is looked at
    /// again.
    fn sort_classes(&self, rows: &mut [Row]) {
        let natives = self.shared.classes.len();
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
                    .class_decl(*class)
                    .map(|decl| {
                        decl.parent
                            .iter()
                            .chain(decl.interfaces.iter())
                            .filter_map(|name| self.class_named(&name.0))
                            .filter(|class| usize::try_from(class.0).is_ok_and(|c| c >= natives))
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
            .filter_map(user_function_decl)
            .filter(|decl| !decl.name.0.starts_with(b"{"))
            .map(|decl| decl.name.0.to_vec())
            .rev()
            .collect();
        let classes = rows
            .iter()
            .map(|row| match row {
                Row::Class(class) => self
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

/// The class numbered `at`.
fn class_id(at: usize) -> ClassId {
    ClassId(u32::try_from(at).unwrap_or(u32::MAX))
}

/// The four tables a request starts from: the preload's rows where the
/// engine preloaded, and none where it did not.
pub(crate) struct Tables {
    pub(crate) classes: Keyed<Declared>,
    pub(crate) aliases: Keyed<Alias>,
    pub(crate) functions: Stacked<UserFunction>,
    pub(crate) enum_functions: Stacked<(ClassId, EnumMethod)>,
}

impl Tables {
    pub(crate) fn of(preloaded: Option<&Preloaded>) -> Tables {
        match preloaded {
            Some(preloaded) => Tables {
                classes: Keyed::over(&preloaded.classes),
                aliases: Keyed::over(&preloaded.aliases),
                functions: Stacked::over(Arc::clone(&preloaded.functions)),
                enum_functions: Stacked::over(Arc::clone(&preloaded.enum_functions)),
            },
            None => Tables {
                classes: Keyed::default(),
                aliases: Keyed::default(),
                functions: Stacked::default(),
                enum_functions: Stacked::default(),
            },
        }
    }
}

impl Request {
    /// The preload's file opened nothing: `php_stream_open_for_zend`'s
    /// warning with no function to name, and the `require`'s fatal, at
    /// no file and no line, as php's startup raises them.
    pub(crate) fn preload_unopened(&mut self, script: &[u8]) {
        let include_path =
            String::from_utf8_lossy(self.ini_get(b"include_path").unwrap_or_default()).into_owned();
        crate::api::raising::raise_at(
            self,
            Level::Warning,
            "PHP Startup: Failed to open stream: No such file or directory",
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

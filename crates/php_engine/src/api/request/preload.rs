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

use super::{Declared, PreloadStatistics, Request, UserFunction, user_function_decl};
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

/// An alias row of the table of classes: the bucket it stands in, the
/// lowered name, and the class it names.
pub(crate) type Alias = (usize, Vec<u8>, ClassId);

/// What the preload left for every request (design §6.3):
/// `ZCSG(preload_script)` and `ZCSG(saved_scripts)`. Built once by the
/// preload's request and read by every request after; nothing in it
/// changes but the scripts' hit counters.
pub(crate) struct Preloaded {
    pub(crate) classes: Arc<[Declared]>,
    pub(crate) aliases: Arc<[Alias]>,
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
        let unit = shared.publish_file(&source, path, &self.cwd, self.stack_limit());
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
        self.bind_declared(&candidate.unit, candidate.at, linking.composed);
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
            classes: self.declared.take_own().into(),
            aliases: self.aliases.take_own().into(),
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
        let mut placed: Vec<(usize, Row)> = (natives..)
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
        self.buckets = 0;
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
                    let place = self.bucket();
                    self.declared.push(Declared { place, ..declared });
                }
                Row::Alias(alias, old) => {
                    let class = renumbered
                        .iter()
                        .find(|(from, _)| from == old)
                        .map_or(*old, |(_, to)| *to);
                    let place = self.bucket();
                    self.aliases.push((place, alias.clone(), class));
                }
                Row::Unlinked(_) => {}
            }
        }
        self.user_functions.extend(functions);
        for (_, class) in renumbered {
            self.bind_members(class);
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
    pub(crate) classes: Stacked<Declared>,
    pub(crate) aliases: Stacked<Alias>,
    pub(crate) functions: Stacked<UserFunction>,
    pub(crate) enum_functions: Stacked<(ClassId, EnumMethod)>,
}

impl Tables {
    pub(crate) fn of(preloaded: Option<&Preloaded>) -> Tables {
        match preloaded {
            Some(preloaded) => Tables {
                classes: Stacked::over(Arc::clone(&preloaded.classes)),
                aliases: Stacked::over(Arc::clone(&preloaded.aliases)),
                functions: Stacked::over(Arc::clone(&preloaded.functions)),
                enum_functions: Stacked::over(Arc::clone(&preloaded.enum_functions)),
            },
            None => Tables {
                classes: Stacked::default(),
                aliases: Stacked::default(),
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

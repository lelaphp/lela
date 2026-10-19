//! A class in the table while its link is under way, and the variance
//! obligations `zend_do_link_class` leaves on it (`Z/Zend/zend_inheritance.c:3526`).
//! php binds a class under its name before it links it, so a class its
//! link autoloads, which runs the program, can name it: a variance
//! check reads it (`ZEND_FETCH_CLASS_ALLOW_UNLINKED`), and once its own
//! checks are made but for the ones that need a class not declared yet
//! it is nearly linked, which a parent or an interface may be
//! (`ZEND_FETCH_CLASS_ALLOW_NEARLY_LINKED`). Its link then autoloads
//! each class a check registered (`load_delayed_classes`) and checks
//! again what it left (`resolve_delayed_variance_obligations`), a child
//! whose parent or interface is nearly linked settling that one's
//! first. Every other lookup finds neither.
//!
//! The obligations are not held: [`php_link::link`] reads the
//! table as it stands, and adding classes to it only settles what was
//! unresolved, so linking the class again when its obligations are due
//! leaves the same checks, now settled, in the same order, with the
//! first one that still fails first. The class stands here until then,
//! out of the request's table of linked classes, whose every class has
//! only linked classes above it.

use std::cell::RefCell;
use std::sync::Arc;

use php_link::{Composed, Held, Linked, Linking, Obligation, Refused};
use php_unit::Level;
use php_unit::PublishedUnit;
use php_unit::decl::{Bare, ClassDecl, ClassKind, ConstExpr, Name};

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Key, Value, ValueRef, walk};

/// The classes whose link is under way, `CG(delayed_autoloads)`, and
/// the units the cache served the request from what it holds, whose
/// declarations are php's immutable ones (`ZEND_ACC_IMMUTABLE`), the
/// only ones the inheritance cache keeps a link of.
#[derive(Default)]
pub(crate) struct Links {
    unlinked: Vec<Unlinked>,
    autoloads: Vec<Vec<u8>>,
    held: Vec<PublishedUnit>,
}

/// What a link read of the table beside what it linked: each class a
/// check looked up by name, under the name as the declaration wrote it,
/// which is the key `track_class_dependency` files it under and the name
/// an inheritance cache's autoload hands the autoloader, with the class
/// it found; whether a check read a class whose link is under way; and
/// whether the traits' composition compared values the request
/// evaluated.
#[derive(Default)]
pub(super) struct Read {
    names: Vec<(Box<[u8]>, Option<ClassId>)>,
    unlinked: bool,
    evaluated: bool,
}

/// One class the table holds under its name that is not
/// `ZEND_ACC_LINKED`: its declaration's unit and index, and how far its
/// link has come.
struct Unlinked {
    unit: PublishedUnit,
    at: u32,
    stage: Stage,
}

enum Stage {
    /// Its parent, traits and interfaces are being fetched, and a fetch
    /// that fails takes it out of the table again, unless a variance
    /// check has read it meanwhile (`CG(unlinked_uses)`).
    Fetching { used: bool },
    /// `ZEND_ACC_NEARLY_LINKED`: linked but for its obligations, and
    /// composed with its traits as the link composed it.
    Nearly { composed: Option<Box<Composed>> },
}

impl Unlinked {
    fn decl(&self) -> Option<&ClassDecl> {
        match &self.stage {
            Stage::Nearly {
                composed: Some(composed),
            } => Some(&composed.decl),
            _ => self.unit.classes.get(usize::try_from(self.at).ok()?),
        }
    }
}

impl Request {
    fn unlinked_named(&self, name: Bare<'_>) -> Option<usize> {
        self.links.unlinked.iter().position(|unlinked| {
            unlinked
                .decl()
                .is_some_and(|decl| decl.name.0.eq_ignore_ascii_case(name.bytes()))
        })
    }

    /// Whether the table holds `name` for a class whose link is under
    /// way, which `zend_lookup_class_ex` without
    /// `ZEND_FETCH_CLASS_ALLOW_UNLINKED` finds and neither answers nor
    /// autoloads.
    pub(crate) fn link_under_way(&self, name: Bare<'_>) -> bool {
        self.unlinked_named(name).is_some()
    }

    /// The declaration of the class `name` whose link is under way.
    pub(super) fn unlinked_decl(&self, name: &[u8]) -> Option<&ClassDecl> {
        self.links
            .unlinked
            .get(self.unlinked_named(Bare::resolved(name))?)?
            .decl()
    }

    /// Whether `name` is `ZEND_ACC_NEARLY_LINKED`, which a parent or an
    /// interface fetched for a link may be.
    pub(super) fn nearly_linked(&self, name: &[u8]) -> bool {
        self.unlinked_named(Bare::resolved(name))
            .and_then(|at| self.links.unlinked.get(at))
            .is_some_and(|unlinked| matches!(unlinked.stage, Stage::Nearly { .. }))
    }

    /// The class at `at` of `unit` into the table under its name before
    /// its link fetches what it names (`zend_bind_class_in_slot`).
    pub(super) fn link_begun(&mut self, unit: &PublishedUnit, at: u32) {
        self.links.unlinked.push(Unlinked {
            unit: PublishedUnit::clone(unit),
            at,
            stage: Stage::Fetching { used: false },
        });
    }

    /// A fetch for the link of `name` failed with `thrown`: the class out
    /// of the table again, which is what lets the program catch it,
    /// unless a variance check read it while it stood there, when php's
    /// `check_unrecoverable_load_failure` makes the throwable a fatal.
    /// A trait's fetch is not checked so.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { throw new Exception("no $c"); });
    /// try { class B extends A {} } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// interface I {}
    /// spl_autoload_register(function () {
    ///     class X { function f(): I {} }
    ///     class Y extends X { function f(): B {} }
    /// }, true, true);
    /// class B extends A implements I {}
    /// ```
    /// ```expect exit=255
    /// no A
    ///
    /// Fatal error: During inheritance of B with variance dependencies: Uncaught Exception: no A in %s:2
    /// Stack trace:
    /// #0 %s(9): {closure:%s:2}('A')
    /// #1 {main} in %s on line 9
    /// ```
    pub(super) fn link_abandoned(&mut self, name: &[u8], thrown: Thrown, checked: bool) -> Thrown {
        let Some(at) = self.unlinked_named(Bare::resolved(name)) else {
            return thrown;
        };
        let unlinked = self.links.unlinked.remove(at);
        if checked && matches!(unlinked.stage, Stage::Fetching { used: true }) {
            let prefix = format!(
                "During inheritance of {} with variance dependencies",
                String::from_utf8_lossy(name)
            );
            return self.uncaught_during(&prefix, thrown);
        }
        thrown
    }

    /// `zend_exception_uncaught_error`: the throwable `thrown` carries,
    /// cleared and rendered behind `prefix` as `E_ERROR` where the
    /// request stands. A `Thrown` with none is a fatal already raised.
    fn uncaught_during(&mut self, prefix: &str, thrown: Thrown) -> Thrown {
        let Some(exception) = thrown.exception().copied() else {
            return thrown;
        };
        if self.pending() == Some(exception) {
            self.take_pending_view();
        }
        // `zval_get_string` over the throwable: its class's own
        // `__toString`, which a user class may override.
        let rendered = match crate::ops::to_string(self, &exception.as_value()) {
            Ok(string) => {
                let text = String::from_utf8_lossy(string.as_bytes()).into_owned();
                self.release(crate::api::values::Value::string(string));
                text
            }
            Err(inner) => {
                self.release(exception.as_value());
                return inner;
            }
        };
        self.release(exception.as_value());
        crate::api::raising::fatal(self, &format!("{prefix}: Uncaught {rendered}"))
    }

    /// [`php_link::link`] of `decl` over the table as a variance
    /// check reads it: the linked classes and those whose link is under
    /// way, a class still fetching marked as read.
    pub(super) fn link_against_table(&mut self, decl: &ClassDecl) -> Linking<ClassId> {
        self.link_reading(decl, false).0
    }

    /// [`Request::link_against_table`] beside what the link read
    /// ([`Read`]), which the inheritance cache keys a published link by.
    /// `early` is the bind opcache's load makes,
    /// `zend_accel_do_delayed_early_binding`, with the class under its
    /// runtime definition key alone: [`php_link::early_bind`], where a
    /// check from a scope other than the class's own does not find it by
    /// its name and leaves an obligation.
    pub(super) fn link_reading(
        &mut self,
        decl: &ClassDecl,
        early: bool,
    ) -> (Linking<ClassId>, Read) {
        let values = self.trait_values(decl);
        let read = RefCell::new(Vec::new());
        // What a check read of another class than the one linking:
        // `track_class_dependency` passes over the class itself.
        let names = RefCell::new(Vec::new());
        let foreign = std::cell::Cell::new(false);
        let linking = {
            let lookup = |name: &[u8]| {
                let own = name.eq_ignore_ascii_case(&decl.name.0);
                let written: Box<[u8]> = name.into();
                if let Some(held) = self.held_class(Bare::resolved(name)) {
                    let found = held.record.as_ref().map(|(class, _)| *class);
                    if !own {
                        names.borrow_mut().push((written, found));
                    }
                    return Some(held);
                }
                if !own {
                    names.borrow_mut().push((written, None));
                }
                let at = self.unlinked_named(Bare::resolved(name))?;
                let unlinked = self.links.unlinked.get(at)?;
                read.borrow_mut().push(at);
                foreign.set(foreign.get() || !own);
                let bodies = match &unlinked.stage {
                    Stage::Nearly {
                        composed: Some(composed),
                    } => Some(composed.bodies.as_slice()),
                    _ => None,
                };
                Some(Held {
                    decl: unlinked.decl()?,
                    bodies,
                    linked: false,
                    record: None,
                })
            };
            let holder = |holder: ClassId| self.class_decl(holder);
            let evaluated = |expr: &ConstExpr| {
                values
                    .iter()
                    .find(|(written, _)| written == expr)
                    .map(|(_, value)| value.clone())
            };
            if early && decl.traits.is_empty() {
                php_link::early_bind(decl, &lookup, &holder)
            } else {
                php_link::link(decl, &lookup, &holder, &evaluated)
            }
        };
        let read = read.into_inner();
        let reading = Read {
            names: names.into_inner(),
            unlinked: foreign.get(),
            evaluated: !values.is_empty(),
        };
        for at in read {
            if let Some(Unlinked {
                stage: Stage::Fetching { used },
                ..
            }) = self.links.unlinked.get_mut(at)
            {
                *used = true;
            }
        }
        (linking, reading)
    }

    /// `zval_update_constant_ex` over what the traits' composition
    /// compares (`zend_do_traits_property_binding`,
    /// `do_trait_constant_check`): each property default and each
    /// constant value that is no plain value, of a name the class and
    /// its traits declare more than once, evaluated to one with the
    /// others of its name, which the linker compares in their place. A
    /// value that is no plain value (an object) or that cannot be evaluated before the class is
    /// declared is left out and compared as written.
    fn trait_values(&mut self, decl: &ClassDecl) -> Vec<(ConstExpr, ConstExpr)> {
        if decl.traits.is_empty() {
            return Vec::new();
        }
        let mut properties: Vec<(Box<[u8]>, Option<ConstExpr>)> = decl
            .properties
            .iter()
            .map(|p| (p.name.0.clone(), p.default.clone()))
            .collect();
        let mut constants: Vec<(Box<[u8]>, ConstExpr)> = decl
            .constants
            .iter()
            .map(|c| (c.name.0.clone(), c.value.clone()))
            .collect();
        for name in &decl.traits {
            let Some(used) = self
                .class_named(name.into())
                .and_then(|c| self.class_decl(c))
            else {
                continue;
            };
            properties.extend(
                used.properties
                    .iter()
                    .map(|p| (p.name.0.clone(), p.default.clone())),
            );
            constants.extend(
                used.constants
                    .iter()
                    .map(|c| (c.name.0.clone(), c.value.clone())),
            );
        }
        // Each name declared more than once whose values are not all
        // literals has all of them evaluated, so the two sides compared
        // are both values.
        let mut wanted: Vec<ConstExpr> = Vec::new();
        let property_rows: Vec<(&[u8], &ConstExpr)> = properties
            .iter()
            .filter_map(|(name, default)| default.as_ref().map(|d| (&**name, d)))
            .collect();
        let constant_rows: Vec<(&[u8], &ConstExpr)> = constants
            .iter()
            .map(|(name, value)| (&**name, value))
            .collect();
        for rows in [&property_rows, &constant_rows] {
            for (name, _) in rows {
                let group: Vec<&ConstExpr> = rows
                    .iter()
                    .filter(|(other, _)| other == name)
                    .map(|(_, expr)| *expr)
                    .collect();
                if group.len() > 1 && group.iter().any(|expr| !is_literal(expr)) {
                    wanted.extend(group.into_iter().cloned());
                }
            }
        }
        let mut values: Vec<(ConstExpr, ConstExpr)> = Vec::new();
        for expr in wanted {
            if values.iter().any(|(written, _)| *written == expr) {
                continue;
            }
            match crate::api::call::eval_const_expr_made(self, &expr, None) {
                Ok(value) => {
                    if let Some(literal) = literal_of(self, &value) {
                        values.push((expr, literal));
                    }
                    self.release(value);
                }
                // A value the class's own scope would give, `self::X`,
                // cannot be read before the class is: it is compared as
                // written, which is what php's comparison of the two
                // values finds where both name the one constant.
                Err(thrown) => {
                    if let Some(object) = thrown.exception() {
                        self.release(object.as_value());
                    }
                }
            }
        }
        values
    }

    /// The end of `zend_do_link_class` for the class at `at` of `unit`,
    /// which `linking` linked: refused at the site php names; bound where
    /// no obligation is left; and otherwise nearly linked, the classes
    /// its checks registered autoloaded and its obligations checked
    /// once they are, unless a class that depends on it checked them
    /// first. What the link raised is emitted at its end, or before a
    /// fatal ends the request.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) {
    ///     echo "load $c\n";
    ///     if ($c === 'Y') { class Y extends B {} }
    ///     if ($c === 'X') { class X {} }
    /// });
    /// class A { function f(): X {} }
    /// class B extends A { function f(): Y {} }
    /// echo "linked\n";
    /// ```
    /// ```expect exit=255
    /// load Y
    /// load X
    ///
    /// Fatal error: Declaration of B::f(): Y must be compatible with A::f(): X in %s on line 8
    /// ```
    pub(super) fn link_settled(
        &mut self,
        unit: &PublishedUnit,
        at: u32,
        linking: Linking<ClassId>,
    ) -> Result<(), Thrown> {
        let Linking {
            outcome,
            raised,
            composed,
            obligations,
            autoloads,
            record,
        } = linking;
        // What the link raised, held until its end, or until the end
        // of the record that stood when it began: a file's compile.
        let recording = self.begin_recording();
        for raised in raised {
            self.note_error_in(
                raised.level(),
                &raised.to_string(),
                &raised.site().file.0,
                raised.site().line,
            );
        }
        if let Linked::Refused(refused) = outcome {
            return Err(self.link_refused(&refused));
        }
        self.pause_recording(recording);
        let name = unit
            .classes
            .get(usize::try_from(at).unwrap_or(usize::MAX))
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let standing = self.unlinked_named(Bare::resolved(&name));
        if obligations.is_empty() || standing.is_none() {
            if let Some(standing) = standing {
                self.links.unlinked.remove(standing);
            }
            self.bind_declared(unit, at, composed, record);
            self.emit_recorded(recording);
            return Ok(());
        }
        if let Some(unlinked) = standing.and_then(|at| self.links.unlinked.get_mut(at)) {
            unlinked.stage = Stage::Nearly {
                composed: composed.map(Box::new),
            };
        }
        for name in autoloads {
            if !self.links.autoloads.contains(&name) {
                self.links.autoloads.push(name);
            }
        }
        self.load_delayed_classes(&name)?;
        if self.nearly_linked(&name) {
            self.resolve_obligations(&name)?;
        }
        self.emit_recorded(recording);
        Ok(())
    }

    /// `load_delayed_classes`: each class a check registered, the first
    /// first, looked up as the program looks one up, so autoloaded;
    /// what an autoloader throws ends the request, since the class
    /// cannot leave the table any more.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { throw new Exception("no $c"); });
    /// class A { function f(): object {} }
    /// class B extends A { function f(): Y {} }
    /// ```
    /// ```expect exit=255
    /// Fatal error: During inheritance of B, while autoloading Y: Uncaught Exception: no Y in %s:2
    /// Stack trace:
    /// #0 %s(4): {closure:%s:2}('Y')
    /// #1 {main} in %s on line 4
    /// ```
    fn load_delayed_classes(&mut self, class: &[u8]) -> Result<(), Thrown> {
        while !self.links.autoloads.is_empty() {
            let name = self.links.autoloads.remove(0);
            if let Err(thrown) = crate::api::call::autoload(self, &name) {
                let prefix = format!(
                    "During inheritance of {}, while autoloading {}",
                    String::from_utf8_lossy(class),
                    String::from_utf8_lossy(&name)
                );
                return Err(self.uncaught_during(&prefix, thrown));
            }
        }
        Ok(())
    }

    /// `resolve_delayed_variance_obligations` of the nearly linked class
    /// `name`: linked again over the table as it stands, each
    /// obligation in order, a dependency still nearly linked settled
    /// first and a check still unresolved refused, then bound.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) {
    ///     if ($c === 'A') { class A { function m(): B {} } echo "A\n"; }
    ///     elseif ($c === 'B') { class B extends A { function m(): C {} } echo "B\n"; }
    ///     else { class C extends B {} echo "C\n"; }
    /// });
    /// new B;
    /// var_dump(class_exists('C', false));
    /// ```
    /// ```expect
    /// A
    /// C
    /// B
    /// bool(true)
    /// ```
    fn resolve_obligations(&mut self, name: &[u8]) -> Result<(), Thrown> {
        let Some(at) = self.unlinked_named(Bare::resolved(name)) else {
            return Ok(());
        };
        let Some(unlinked) = self.links.unlinked.get(at) else {
            return Ok(());
        };
        let (unit, at) = (PublishedUnit::clone(&unlinked.unit), unlinked.at);
        let Some(decl) = unit.classes.get(usize::try_from(at).unwrap_or(usize::MAX)) else {
            return Ok(());
        };
        let linking = self.link_against_table(decl);
        for obligation in &linking.obligations {
            match obligation {
                Obligation::Dependency(dependency) => {
                    if self.nearly_linked(&dependency.0) {
                        self.resolve_obligations(&dependency.0)?;
                    }
                }
                Obligation::Unresolved(refused) => return Err(self.link_refused(refused)),
            }
        }
        if let Linked::Refused(refused) = &linking.outcome {
            return Err(self.link_refused(refused));
        }
        if let Some(standing) = self.unlinked_named(Bare::resolved(name)) {
            self.links.unlinked.remove(standing);
        }
        self.bind_declared(&unit, at, linking.composed, linking.record);
        Ok(())
    }

    /// What a link refused, at its level and site, the request ended in
    /// it after what the links recorded, which the fatal emits first.
    pub(super) fn link_refused(&mut self, refused: &Refused) -> Thrown {
        let message = refused.refusal.to_string();
        match refused.refusal.level() {
            Level::CoreError => self.end_in_core_error(&message),
            level => {
                self.end_in_compile_time(level, &message, &refused.site.file.0, refused.site.line);
            }
        }
        Thrown::of(None)
    }
}

/// A constant expression that is a value as written: `null`, a bool,
/// a number, a string, or the array the compiler folded.
pub(super) fn is_literal(expr: &ConstExpr) -> bool {
    matches!(
        expr,
        ConstExpr::Null
            | ConstExpr::Bool(_)
            | ConstExpr::Long { .. }
            | ConstExpr::Double(_)
            | ConstExpr::Str(_)
            | ConstExpr::ImmutableArray(_)
    )
}

/// A value as the constant expression that writes it, arrays element by
/// element with their keys; none for an object or a resource.
fn literal_of(req: &Request, value: &Value) -> Option<ConstExpr> {
    Some(match value.deref().as_ref() {
        ValueRef::Null | ValueRef::Undef => ConstExpr::Null,
        ValueRef::Bool(b) => ConstExpr::Bool(b),
        ValueRef::Int(i) => ConstExpr::long(i),
        ValueRef::Float(d) => ConstExpr::Double(d),
        ValueRef::Str(s) => ConstExpr::Str(s.as_bytes().into()),
        ValueRef::Array(array) => {
            let mut elements = Vec::new();
            for (key, element) in walk::rows(req, *array) {
                let key = match key {
                    Key::Int(i) => ConstExpr::long(i),
                    Key::Str(s) => ConstExpr::Str(s.as_bytes().into()),
                };
                elements.push((Some(key), literal_of(req, &element.deref())?));
            }
            ConstExpr::Array { elements, line: 0 }
        }
        _ => return None,
    })
}

/// The classes a link fetched before it checked anything, the key an
/// inheritance cache's link is found by beside the declaration: the
/// parent, then the traits and the interfaces in the order written.
pub(super) struct Fetched {
    parent: Option<ClassId>,
    uses: Box<[ClassId]>,
}

impl Request {
    /// A unit the cache served this request from what it holds.
    pub(super) fn note_held(&mut self, unit: &PublishedUnit) {
        if !self
            .links
            .held
            .iter()
            .any(|held| PublishedUnit::ptr_eq(held, unit))
        {
            self.links.held.push(PublishedUnit::clone(unit));
        }
    }

    /// What `decl`'s link fetched, each a class every request shares;
    /// none where one is the request's own, or not linked, which no
    /// cached link can be keyed by (`UPDATE_IS_CACHEABLE`), or where the
    /// declaration is no held unit's, which php's is not immutable, or is
    /// an enum's, whose internal methods php will not cache.
    pub(super) fn fetched(&self, unit: &PublishedUnit, decl: &ClassDecl) -> Option<Fetched> {
        if decl.kind == ClassKind::Enum
            || !self
                .links
                .held
                .iter()
                .any(|held| PublishedUnit::ptr_eq(held, unit))
        {
            return None;
        }
        let shared = |name: &Name| {
            self.class_named(name.into())
                .filter(|class| class.is_published())
        };
        let parent = match &decl.parent {
            Some(name) => Some(shared(name)?),
            None => None,
        };
        let uses = (decl.traits.iter())
            .chain(&decl.interfaces)
            .map(shared)
            .collect::<Option<Box<[ClassId]>>>()?;
        Some(Fetched { parent, uses })
    }

    /// `zend_accel_inheritance_cache_get`: the link of the declaration at
    /// `at` of `unit` an earlier request published against the classes
    /// `fetched` names, where each class its checks read by name is,
    /// looked up without autoloading, the one it read then. One whose
    /// dependency the table does not hold yet is `needs_autoload`: each
    /// dependency is autoloaded, and the entry looked at again. None
    /// where no entry holds, which links the class afresh; what an
    /// autoloader threw is the `Err`.
    ///
    /// Served by the built-in server, whose requests share the cache: the
    /// third request declares a `D` of another file, so `B`'s check of
    /// `f(): D` against `f(): Base` reads another class, the link the
    /// first published is not the third's, and the third links afresh
    /// and is refused. `B` implements an interface, so it is linked as
    /// the program runs (`zend_do_link_class`), which reads its variance
    /// dependencies; a class bound early at the load is not checked
    /// again (`zend_try_early_bind`), and keeps no dependency:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/inherit' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/base.php", '<?php class Base {} interface I {} class A { function f(): Base { return new Base; } }');
    /// file_put_contents("$dir/d1.php", '<?php class D extends Base {}');
    /// file_put_contents("$dir/d2.php", '<?php class D {}');
    /// file_put_contents("$dir/b.php", '<?php class B extends A implements I { function f(): D { return new D; } }');
    /// file_put_contents("$dir/index.php", '<?php require __DIR__ . "/base.php"; require __DIR__ . "/d" . $_GET["d"] . ".php"; require __DIR__ . "/b.php"; echo get_class((new B)->f()), "\n";');
    /// foreach (glob("$dir/*.php") as $file) { touch($file, time() - 10); }
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-d', 'html_errors=0', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['file', "$dir/log", 'w']], $pipes);
    /// for ($i = 0, $line = ''; $i < 300 && !str_contains($line, 'started'); $i++) { usleep(100000); $line = (string) @file("$dir/log")[0]; }
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach ([1, 1, 2] as $d) { echo preg_replace('/ in \S+ on/', ' in %s on', file_get_contents("http://127.0.0.1:$port/?d=$d")); }
    /// proc_terminate($p); proc_close($p);
    /// array_map('unlink', glob("$dir/*")); rmdir($dir);
    /// ```
    /// ```expect process
    /// D
    /// D
    ///
    /// Fatal error: Declaration of B::f(): D must be compatible with A::f(): Base in %s on line 1
    /// Stack trace:
    /// #0 %s(1): require()
    /// #1 {main}
    /// ```
    ///
    /// The `needs_autoload` path: the first request publishes `B`'s link
    /// with `D` loaded; a later one names `D` to nothing before `B`
    /// links, so the entry's dependency is autoloaded, under the name
    /// `f(): D` wrote, and the link served where the autoloader loads
    /// the `D` it was linked against. An autoloader that loads another
    /// `D` leaves the entry missed and the class linked afresh, refused;
    /// one that declares nothing ends the cache's lookup, and the fresh
    /// link asks the autoloader again and is refused for the class it
    /// cannot find:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/needs' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/base.php", '<?php class Base {} interface I {} class A { function f(): Base { return new Base; } }');
    /// file_put_contents("$dir/d1.php", '<?php class D extends Base {}');
    /// file_put_contents("$dir/d2.php", '<?php class D {}');
    /// file_put_contents("$dir/b.php", '<?php class B extends A implements I { function f(): D { return new D; } }');
    /// file_put_contents("$dir/index.php", '<?php $m = $_GET["m"]; require __DIR__ . "/base.php"; spl_autoload_register(function ($c) use ($m) { echo "autoload $c\n"; if ($m === "own") require __DIR__ . "/d1.php"; if ($m === "other") require __DIR__ . "/d2.php"; }); if ($m === "first") require __DIR__ . "/d1.php"; require __DIR__ . "/b.php"; echo get_class((new B)->f()), "\n";');
    /// foreach (glob("$dir/*.php") as $file) { touch($file, time() - 10); }
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-d', 'html_errors=0', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['file', "$dir/log", 'w']], $pipes);
    /// for ($i = 0, $line = ''; $i < 300 && !str_contains($line, 'started'); $i++) { usleep(100000); $line = (string) @file("$dir/log")[0]; }
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach (['first', 'own', 'other', 'none', 'own'] as $m) { echo "-- $m\n", preg_replace('/ in \S+ on/', ' in %s on', file_get_contents("http://127.0.0.1:$port/?m=$m")); }
    /// proc_terminate($p); proc_close($p);
    /// array_map('unlink', glob("$dir/*")); rmdir($dir);
    /// ```
    /// ```expect process
    /// -- first
    /// D
    /// -- own
    /// autoload D
    /// D
    /// -- other
    /// autoload D
    ///
    /// Fatal error: Declaration of B::f(): D must be compatible with A::f(): Base in %s on line 1
    /// Stack trace:
    /// #0 %s(1): require()
    /// #1 {main}
    /// -- none
    /// autoload D
    /// autoload D
    ///
    /// Fatal error: Could not check compatibility between B::f(): D and A::f(): Base, because class D is not available in %s on line 1
    /// Stack trace:
    /// #0 %s(1): require()
    /// #1 {main}
    /// -- own
    /// autoload D
    /// D
    /// ```
    pub(super) fn cached_link(
        &mut self,
        unit: &PublishedUnit,
        at: u32,
        fetched: &Fetched,
    ) -> Result<Option<Arc<crate::inheritance_cache::Entry>>, Thrown> {
        let Some(chain) = self.arena.inheritance.find(unit, at) else {
            return Ok(None);
        };
        // php prepends, so the newest entry is the first it reads.
        for entry in chain.iter().rev() {
            if entry.parent != fetched.parent || entry.uses != fetched.uses {
                continue;
            }
            let mut autoloaded = false;
            loop {
                let mut needs_autoload = false;
                let mut holds = true;
                for (name, class) in &entry.dependencies {
                    match self.class_named(Bare::resolved(name)) {
                        Some(found) if found == *class => {}
                        Some(_) => {
                            holds = false;
                            break;
                        }
                        None => needs_autoload = true,
                    }
                }
                if !holds {
                    break;
                }
                if !needs_autoload {
                    return Ok(Some(Arc::clone(entry)));
                }
                if autoloaded {
                    return Ok(None);
                }
                for (name, _) in &entry.dependencies {
                    if crate::api::call::autoload(self, name)?.is_none() {
                        return Ok(None);
                    }
                }
                autoloaded = true;
            }
        }
        Ok(None)
    }

    /// `zend_inheritance_cache_add`: the class `linking` linked at `at` of
    /// `unit` published ([`Request::publish`]) and its link kept, where
    /// it is php's to cache: linked with no obligation left, every class
    /// its checks read one every request shares, none whose link was
    /// under way, and no value of the request's compared. A class bound
    /// early as its unit loads keeps no dependency, as `zend_try_early_bind`
    /// links it with no class tracked (`CG(current_linking_class)` unset
    /// while `zend_can_early_bind` checks it), so a later request finds it
    /// by its parent alone. None where it is not php's to cache, or the
    /// arena has no room, which leaves the class to bind as the request's
    /// own.
    ///
    /// Served by the built-in server: `B` is bound early as `b.php`
    /// loads, so the third request, whose `D` is another file's, is
    /// served the link the first published, as php's is:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/early' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/base.php", '<?php class Base {} class A { function f(): Base { return new Base; } }');
    /// file_put_contents("$dir/d1.php", '<?php class D extends Base {}');
    /// file_put_contents("$dir/d2.php", '<?php class D {}');
    /// file_put_contents("$dir/b.php", '<?php class B extends A { function f(): D { return new D; } }');
    /// file_put_contents("$dir/index.php", '<?php require __DIR__ . "/base.php"; require __DIR__ . "/d" . $_GET["d"] . ".php"; require __DIR__ . "/b.php"; echo get_class((new B)->f()), "\n";');
    /// foreach (glob("$dir/*.php") as $file) { touch($file, time() - 10); }
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-d', 'html_errors=0', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['file', "$dir/log", 'w']], $pipes);
    /// for ($i = 0, $line = ''; $i < 300 && !str_contains($line, 'started'); $i++) { usleep(100000); $line = (string) @file("$dir/log")[0]; }
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach ([1, 1, 2] as $d) { echo file_get_contents("http://127.0.0.1:$port/?d=$d"); }
    /// proc_terminate($p); proc_close($p);
    /// array_map('unlink', glob("$dir/*")); rmdir($dir);
    /// ```
    /// ```expect process
    /// D
    /// D
    /// D
    /// ```
    pub(super) fn publish_link(
        &mut self,
        unit: &PublishedUnit,
        at: u32,
        fetched: Fetched,
        linking: &Linking<ClassId>,
        read: &Read,
        early: bool,
    ) -> Option<Arc<crate::inheritance_cache::Entry>> {
        if !matches!(linking.outcome, Linked::Linked)
            || !linking.obligations.is_empty()
            || read.unlinked
            || read.evaluated
        {
            return None;
        }
        let mut dependencies: Vec<(Box<[u8]>, ClassId)> = Vec::new();
        for (name, found) in read.names.iter().filter(|_| !early) {
            let class = (*found).filter(|class| class.is_published())?;
            // `track_class_dependency` leaves out the engine's own classes,
            // which are the same in every request.
            if self.native_class_at(class).is_some()
                || dependencies.iter().any(|(seen, _)| seen == name)
            {
                continue;
            }
            dependencies.push((name.clone(), class));
        }
        let record = linking.record.clone()?;
        let class = self.publish(unit, at, linking.composed.clone(), record, fetched.parent)?;
        let entry = crate::inheritance_cache::Entry {
            parent: fetched.parent,
            uses: fetched.uses,
            dependencies: dependencies.into(),
            class,
            raised: linking.raised.clone().into(),
        };
        Some(self.arena.inheritance.add(unit, at, entry))
    }

    /// The class at `at` of `unit` declared as the published class
    /// `entry` holds, what its link raised raised again
    /// (`zend_emit_recorded_errors_ex`) where a link of it raises them,
    /// at its end, and the class out of the links under way. A link that
    /// raised a deprecation raises it on every request the cache serves
    /// it to:
    ///
    /// ```php
    /// $dir = sys_get_temp_dir() . '/replay' . bin2hex(random_bytes(8));
    /// mkdir($dir);
    /// file_put_contents("$dir/index.php", '<?php class C implements Countable { function count() { return 0; } } echo count(new C), "\n";');
    /// touch("$dir/index.php", time() - 10);
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable=1', '-d', 'opcache.jit=off', '-d', 'html_errors=0', '-S', '127.0.0.1:0', '-t', $dir], [1 => ['file', '/dev/null', 'w'], 2 => ['file', "$dir/log", 'w']], $pipes);
    /// for ($i = 0, $line = ''; $i < 300 && !str_contains($line, 'started'); $i++) { usleep(100000); $line = (string) @file("$dir/log")[0]; }
    /// $port = preg_match('/:(\d+)\) started$/', rtrim($line), $m) ? $m[1] : 0;
    /// foreach ([1, 2] as $n) { echo preg_replace('/ in \S+ on/', ' in %s on', file_get_contents("http://127.0.0.1:$port/")); }
    /// proc_terminate($p); proc_close($p);
    /// array_map('unlink', glob("$dir/*")); rmdir($dir);
    /// ```
    /// ```expect process
    ///
    /// Deprecated: Return type of C::count() should either be compatible with Countable::count(): int, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 1
    /// 0
    ///
    /// Deprecated: Return type of C::count() should either be compatible with Countable::count(): int, or the #[\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice in %s on line 1
    /// 0
    /// ```
    pub(super) fn declare_cached(
        &mut self,
        unit: &PublishedUnit,
        at: u32,
        decl: &ClassDecl,
        entry: &crate::inheritance_cache::Entry,
    ) {
        let recording = self.begin_recording();
        for raised in &entry.raised {
            self.note_error_in(
                raised.level(),
                &raised.to_string(),
                &raised.site().file.0,
                raised.site().line,
            );
        }
        self.pause_recording(recording);
        if let Some(standing) = self.unlinked_named((&decl.name).into()) {
            self.links.unlinked.remove(standing);
        }
        self.declare_published(unit, at, entry.class, decl);
        self.emit_recorded(recording);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Request;
    use crate::api::abi::{Admission, ExtensionBuilder, Lookup, Validation};
    use crate::api::values::ClassId;
    use crate::engine::{Engine, RequestDescription};

    /// An engine whose every file is held as opcache holds it.
    fn engine() -> Engine {
        let held = ExtensionBuilder::default()
            .admission(|_, _| Admission::Held {
                validation: Validation::Never,
                lookup: Lookup::Resolved,
                protection: 0,
                file_cache: None,
            })
            .build("held");
        Engine::cli_builder().extensions(&[held]).build()
    }

    /// A link made as the program serves whose parent, interface and every class its checks
    /// read are published is published once and kept: the second request
    /// links nothing, takes the same number and lays down no row of its
    /// own for the class or its methods. One whose parent is the
    /// request's own is linked in every request and never published.
    #[test]
    fn a_link_over_published_classes_is_published_once_and_one_over_the_request_s_never() {
        let dir = std::env::temp_dir().join(format!("inheritance-{}", std::process::id()));
        assert!(std::fs::create_dir_all(&dir).is_ok());
        let shared = dir.join("shared.php");
        let own = dir.join("own.php");
        let written = [
            (
                &shared,
                "<?php interface I {} class P { function p() {} } \
                       class C extends P implements I { function c(): ?I { return null; } }",
            ),
            (
                &own,
                "<?php class Q extends R implements I { function q() {} }",
            ),
        ];
        for (path, source) in written {
            assert!(std::fs::write(path, source).is_ok());
        }
        let engine = engine();
        let code = format!(
            "require '{}'; class R {{}} require '{}';",
            shared.display(),
            own.display()
        );
        let mut seen = Vec::new();
        for _ in 0..2 {
            let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
            assert!(req.eval_stringl(code.as_bytes(), b"loader").is_ok());
            let c = req.class_named(php_unit::decl::Bare::resolved(b"c"));
            let q = req.class_named(php_unit::decl::Bare::resolved(b"q"));
            assert!(c.is_some_and(ClassId::is_published), "C is published");
            assert!(
                q.is_some_and(|q| !q.is_published()),
                "Q is the request's own"
            );
            let inherited = c.and_then(|c| req.method_named(c, b"p"));
            assert!(inherited.is_some_and(|m| req.user_function(m).is_some()));
            let rows: Vec<String> = (req.own_classes.iter())
                .filter_map(|row| Some(String::from_utf8_lossy(&row.decl()?.name.0).into_owned()))
                .collect();
            assert_eq!(rows, ["R", "Q"], "no row of C's");
            seen.push(c);
        }
        assert_eq!(
            seen.first(),
            seen.last(),
            "the second request took the first's number"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

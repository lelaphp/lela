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
//! The obligations are not held: [`php_unit::link::link`] reads the
//! table as it stands, and adding classes to it only settles what was
//! unresolved, so linking the class again when its obligations are due
//! leaves the same checks, now settled, in the same order, with the
//! first one that still fails first. The class stands here until then,
//! out of the request's table of linked classes, whose every class has
//! only linked classes above it.

use std::cell::RefCell;
use std::sync::Arc;

use php_unit::Level;
use php_unit::Unit;
use php_unit::decl::ClassDecl;
use php_unit::link::{Composed, Held, Linked, Linking, Obligation, Raised, Refused};

use super::Request;
use crate::api::raising::Thrown;

/// The classes whose link is under way, `CG(delayed_autoloads)`, and
/// the diagnostics the links recorded, `EG(errors)`.
#[derive(Default)]
pub(crate) struct Links {
    unlinked: Vec<Unlinked>,
    autoloads: Vec<Vec<u8>>,
    recorded: Vec<Raised>,
}

/// One class the table holds under its name that is not
/// `ZEND_ACC_LINKED`: its declaration's unit and index, and how far its
/// link has come.
struct Unlinked {
    unit: Arc<Unit>,
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
    fn unlinked_named(&self, name: &[u8]) -> Option<usize> {
        let name = name.strip_prefix(b"\\").unwrap_or(name);
        self.links.unlinked.iter().position(|unlinked| {
            unlinked
                .decl()
                .is_some_and(|decl| decl.name.0.eq_ignore_ascii_case(name))
        })
    }

    /// Whether the table holds `name` for a class whose link is under
    /// way, which `zend_lookup_class_ex` without
    /// `ZEND_FETCH_CLASS_ALLOW_UNLINKED` finds and neither answers nor
    /// autoloads.
    pub(crate) fn link_under_way(&self, name: &[u8]) -> bool {
        self.unlinked_named(name).is_some()
    }

    /// The declaration of the class `name` whose link is under way.
    pub(super) fn unlinked_decl(&self, name: &[u8]) -> Option<&ClassDecl> {
        self.links.unlinked.get(self.unlinked_named(name)?)?.decl()
    }

    /// Whether `name` is `ZEND_ACC_NEARLY_LINKED`, which a parent or an
    /// interface fetched for a link may be.
    pub(super) fn nearly_linked(&self, name: &[u8]) -> bool {
        self.unlinked_named(name)
            .and_then(|at| self.links.unlinked.get(at))
            .is_some_and(|unlinked| matches!(unlinked.stage, Stage::Nearly { .. }))
    }

    /// The class at `at` of `unit` into the table under its name before
    /// its link fetches what it names (`zend_bind_class_in_slot`).
    pub(super) fn link_begun(&mut self, unit: &Arc<Unit>, at: u32) {
        self.links.unlinked.push(Unlinked {
            unit: Arc::clone(unit),
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
        let Some(at) = self.unlinked_named(name) else {
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
            self.take_pending();
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
        self.emit_recorded();
        crate::api::raising::fatal(self, &format!("{prefix}: Uncaught {rendered}"))
    }

    /// [`php_unit::link::link`] of `decl` over the table as a variance
    /// check reads it: the linked classes and those whose link is under
    /// way, a class still fetching marked as read.
    pub(super) fn link_against_table(&mut self, decl: &ClassDecl) -> Linking {
        let read = RefCell::new(Vec::new());
        let linking = {
            let lookup = |name: &[u8]| {
                if let Some(held) = self.held_class(name) {
                    return Some(held);
                }
                let at = self.unlinked_named(name)?;
                let unlinked = self.links.unlinked.get(at)?;
                read.borrow_mut().push(at);
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
                })
            };
            php_unit::link::link(decl, &lookup)
        };
        for at in read.into_inner() {
            if let Some(Unlinked {
                stage: Stage::Fetching { used },
                ..
            }) = self.links.unlinked.get_mut(at)
            {
                *used = true;
            }
        }
        linking
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
        unit: &Arc<Unit>,
        at: u32,
        linking: Linking,
    ) -> Result<(), Thrown> {
        let Linking {
            outcome,
            raised,
            composed,
            obligations,
            autoloads,
        } = linking;
        self.links.recorded.extend(raised);
        if let Linked::Refused(refused) = outcome {
            return Err(self.link_refused(&refused));
        }
        let name = unit
            .classes
            .get(usize::try_from(at).unwrap_or(usize::MAX))
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let standing = self.unlinked_named(&name);
        if obligations.is_empty() || standing.is_none() {
            if let Some(standing) = standing {
                self.links.unlinked.remove(standing);
            }
            self.emit_recorded();
            self.bind_declared(unit, at, composed);
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
        self.emit_recorded();
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
        let Some(at) = self.unlinked_named(name) else {
            return Ok(());
        };
        let Some(unlinked) = self.links.unlinked.get(at) else {
            return Ok(());
        };
        let (unit, at) = (Arc::clone(&unlinked.unit), unlinked.at);
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
        if let Some(standing) = self.unlinked_named(name) {
            self.links.unlinked.remove(standing);
        }
        self.bind_declared(&unit, at, linking.composed);
        Ok(())
    }

    /// What a link refused, at its level and site, the request ended in
    /// it after what the links recorded.
    pub(super) fn link_refused(&mut self, refused: &Refused) -> Thrown {
        self.emit_recorded();
        let message = refused.refusal.to_string();
        match refused.refusal.level() {
            Level::CoreError => self.end_in_core_error(&message),
            level => {
                self.end_in_compile_time(level, &message, &refused.site.file.0, refused.site.line);
            }
        }
        Thrown::of(None)
    }

    /// `zend_emit_recorded_errors`: what the links raised, in order.
    pub(super) fn emit_recorded(&mut self) {
        for raised in std::mem::take(&mut self.links.recorded) {
            crate::api::raising::raise_at(
                self,
                raised.level(),
                &raised.to_string(),
                &raised.site().file.0,
                raised.site().line,
            );
        }
    }
}

//! Publishing a linked class into the engine's arena (design §5.14,
//! *Identities*; §6.2, §6.3): [`Request::publish`], the one path every
//! publisher takes, the preload's link, the unit cache's early-bound
//! classes ([`Request::publish_unit`]), and the inheritance cache's link
//! after them. A published class is a row every request reads by its
//! number with nothing taken: its declaration, its record, and its
//! methods and hooks as published functions. What a request changes of
//! it, its static properties, its constants and the defaults whose
//! constant expressions it evaluates, is the request's, kept in the
//! request's tables under the class's number, as php reaches it through
//! `ZEND_MAP_PTR`; nothing of it is written into the row.

use std::sync::Arc;

use php_link::{Composed, LinkedClass};
use php_unit::PublishedUnit;

use super::{Bound, Declared, Request};
use crate::api::values::ClassId;

impl Request {
    /// The class `unit` declares at `at`, composed as `composed` where it
    /// uses traits, published with `record` and `parent`, every class
    /// either names published or the engine's: a number of the arena's
    /// taken, its methods and hooks published as functions under it, its
    /// row filled, and what it builds once and keeps, its placed
    /// properties, built before the number is handed on. None where the
    /// arena has no room for it, which leaves the class to be bound by
    /// the request that asked.
    pub(crate) fn publish(
        &self,
        unit: &PublishedUnit,
        at: u32,
        composed: Option<Composed>,
        record: LinkedClass<ClassId>,
        parent: Option<ClassId>,
    ) -> Option<ClassId> {
        let row = Declared {
            unit: PublishedUnit::clone(unit),
            at,
            composed: composed.map(Box::new),
            bound: Bound::default(),
        };
        let class = self.arena.reserve_class()?;
        let members = self.members_of(&row, class)?;
        let (functions, members) = self.place_published(class, members)?;
        // `zend_build_properties_info_table` at the link: built by the
        // publisher, so no request writes the published row.
        let placed = self.placed_over(class, row.decl()?, Some(&record), parent);
        let row = Declared {
            bound: Bound {
                record: Some(record),
                functions,
                members,
                parent,
                placed,
            },
            ..row
        };
        self.arena.publish_class(class, row).then_some(())?;
        Some(class)
    }

    /// `zend_accel_store_class_table` over a unit the cache holds
    /// (`Z/ext/opcache/zend_persist.c`): each class the compiler bound
    /// early, one with no runtime definition key ([`Unit::runtime_keys`]),
    /// linked once and published with the unit (design §6.3), so every
    /// request that loads the unit declares it by one insert of its
    /// number, as `zend_accel_class_hash_copy` copies the `ce` pointer.
    /// The compiler binds a class early only where its parent is the
    /// engine's or a class it bound early before it in the same file, so
    /// each is linked over the engine's classes and the unit's own
    /// published before it, and never over the request's. By declaration
    /// index, none for a class the unit does not bind early and for one
    /// the arena could not take, which the request binds as its own.
    pub(crate) fn publish_unit(&self, unit: &PublishedUnit) -> Arc<[Option<ClassId>]> {
        let mut published: Vec<Option<ClassId>> = vec![None; unit.classes.len()];
        let mut own: Vec<(Vec<u8>, ClassId)> = Vec::new();
        for (at, decl) in (0u32..).zip(&unit.classes) {
            let keyed = unit.runtime_keys.iter().any(|(_, class)| *class == at);
            if keyed || decl.flags.is_anonymous {
                continue;
            }
            let named = |name: &[u8]| {
                self.shared.class_at(name).or_else(|| {
                    own.iter()
                        .find(|(held, _)| held.eq_ignore_ascii_case(name))
                        .map(|(_, class)| *class)
                })
            };
            let parent = match &decl.parent {
                Some(name) => match named(&name.0) {
                    Some(parent) => Some(parent),
                    None => continue,
                },
                None => None,
            };
            let row = Declared {
                unit: PublishedUnit::clone(unit),
                at,
                composed: None,
                bound: Bound::default(),
            };
            let Some(record) = self.record_over(&row, &named) else {
                continue;
            };
            let Some(class) = self.publish(unit, at, None, record, parent) else {
                continue;
            };
            if let Some(slot) = usize::try_from(at)
                .ok()
                .and_then(|at| published.get_mut(at))
            {
                *slot = Some(class);
            }
            own.push((decl.name.0.to_vec(), class));
        }
        published.into()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::Request;
    use crate::api::values::ClassId;
    use crate::engine::{Engine, RequestDescription};
    use crate::test::Files;

    /// Two requests loading one held file: the classes its compiler bound
    /// early are published once, with the unit, and each request
    /// declares them by their numbers, laying down no row of its own for
    /// a class or a method; a class the file leaves to an opline is the
    /// request's own, as before.
    #[test]
    fn a_held_unit_s_early_bound_classes_are_published_once_and_declared_by_number() {
        let files = Files::new(
            "early-bound",
            &[(
                "classes.php",
                "<?php class P { function p() {} } class C extends P { function c() {} } \
                 class D implements Countable { function count(): int { return 0; } }",
            )],
        );
        let file = files.path().join("classes.php");
        let engine = Engine::builder().build();
        let code = format!("require '{}';", file.display());
        let mut seen = Vec::new();
        for _ in 0..2 {
            let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
            assert!(
                req.code_mut()
                    .eval_stringl(code.as_bytes(), b"loader")
                    .is_ok()
            );
            let (p, c, d) = (
                req.class_named(php_unit::decl::Bare::resolved(b"p")),
                req.class_named(php_unit::decl::Bare::resolved(b"c")),
                req.class_named(php_unit::decl::Bare::resolved(b"d")),
            );
            assert!(p.is_some_and(ClassId::is_published), "P is published");
            assert!(c.is_some_and(ClassId::is_published), "C is published");
            assert!(
                d.is_some_and(|d| !d.is_published()),
                "D is the request's own"
            );
            let inherited = c.and_then(|c| req.symbols().method_named(c, b"p"));
            assert!(inherited.is_some_and(|m| req.user_function(m).is_some()));
            assert_eq!(req.own_classes.len(), 1, "D's row alone");
            let own_methods = req
                .user_functions
                .iter()
                .filter(|f| f.member.is_some())
                .count();
            assert_eq!(own_methods, 1, "D's count alone");
            seen.push((p, c));
        }
        assert_eq!(
            seen.first(),
            seen.last(),
            "published once, the same numbers"
        );
    }
}

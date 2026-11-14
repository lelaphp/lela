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
use php_unit::frozen::Frozen;

use super::{Bound, ClassRow, Declared, InArena, Request};
use crate::api::values::ClassId;
use crate::arena::EpochClassId;

/// What a class being published names itself by in its placed
/// properties before the arena numbers it: a number no class takes,
/// which a frame's header leaves to its called scope's `None`.
const UNNUMBERED: ClassId = ClassId(u32::MAX);

/// Why a class was not published ([`Request::publish`]): either way
/// the class is left to be bound as a request's own, and only a preload,
/// which has no request to leave it to, tells them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unpublished {
    /// The arena refused to hold it: [`Frozen::new`] refused the
    /// declaration the class composed with its traits, or the class
    /// names a class or a body no request but the publisher's can reach.
    Refused,
    /// The arena had no room for it.
    Full,
}

impl Request {
    /// The class `unit` declares at `at`, composed as `composed` where it
    /// uses traits, published with `record` and `parent`, every class
    /// either names published or the engine's: a number of the arena's
    /// taken, its methods and hooks published as functions under it, its
    /// row filled, and what it builds once and keeps, its placed
    /// properties, built before the number is handed on. The parent and
    /// the class are named by the identities the arena minted
    /// ([`EpochClassId`]), so no request's own class is published as one,
    /// and so is every class its record and its placed properties name,
    /// each by the arena's one check or as the class itself, so the row
    /// stands in the arena's scope ([`InArena`]): over the frozen unit,
    /// with the composed declaration frozen by [`Frozen::new`]. A member
    /// whose body is in another unit, a trait's method, is published over
    /// the frozen unit `units` finds that unit to be.
    ///
    /// Everything that can refuse is asked before the arena numbers
    /// anything: the composed declaration, the record's classes, each
    /// member's frozen unit and record, and the placed properties'
    /// classes. So a class refused leaves the arena as it was, and only
    /// its room can stop a publication part way, which leaves numbered
    /// rows no class names, each holding what its record names.
    ///
    /// # Errors
    ///
    /// [`Unpublished::Refused`] where the arena refuses the class, and
    /// [`Unpublished::Full`] where it has no room for it.
    pub(crate) fn publish(
        &self,
        (unit, at): (&Frozen<PublishedUnit>, u32),
        composed: Option<Composed>,
        record: &LinkedClass<ClassId>,
        parent: Option<EpochClassId>,
        units: &dyn Fn(&PublishedUnit) -> Option<Frozen<PublishedUnit>>,
    ) -> Result<EpochClassId, Unpublished> {
        use Unpublished::{Full, Refused};
        let composed = match composed {
            Some(Composed {
                decl,
                bodies,
                abstracts,
            }) => Some(Box::new(Composed {
                decl: Arc::new(Frozen::new(decl).map_err(|_| Refused)?),
                bodies,
                abstracts,
            })),
            None => None,
        };
        let row: Declared<InArena> = Declared {
            unit: Frozen::clone(unit),
            at,
            composed,
            bound: Bound::default(),
        };
        // The record was linked before the class had a number, so it
        // names the class itself nowhere.
        let linked = record.try_map(|named| self.arena.shared_class(named));
        let linked = linked.ok_or(Refused)?;
        let members = self.members_of(ClassRow::Arena(&row)).ok_or(Refused)?;
        let composed = row.composed.as_ref().map(|composed| &composed.decl);
        let members = Request::published_members(members, composed, units).ok_or(Refused)?;
        // `zend_build_properties_info_table` at the link: built by the
        // publisher, so no request writes the published row. The class
        // has no number yet: its own properties are placed under one no
        // class takes, and named by the number once it is taken.
        let decl = row.decl().ok_or(Refused)?;
        let placed = self.placed_over(UNNUMBERED, decl, Some(record), parent.map(ClassId::from));
        let shared =
            |named: ClassId| named == UNNUMBERED || self.arena.shared_class(named).is_some();
        if !placed.names_only(shared) {
            return Err(Refused);
        }
        let class = self.arena.reserve_class().ok_or(Full)?;
        let keyed = |named: ClassId| {
            if named == UNNUMBERED {
                Some(class)
            } else {
                self.arena.shared_class(named)
            }
        };
        // Asked above, and the arena's check answers a class the same
        // once it answered it; were it to answer otherwise, the number is
        // taken, and what stops the publication past it is told as the
        // arena's room is, which leaves a number no class takes.
        let placed = placed.published(keyed).ok_or(Full)?;
        let (functions, members) = self.place_published(class, members).ok_or(Full)?;
        let row = Declared {
            bound: Bound {
                record: Some(linked),
                functions,
                members,
                parent,
                placed,
            },
            ..row
        };
        if !self.arena.publish_class(class, row) {
            return Err(Full);
        }
        Ok(class)
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
    pub(crate) fn publish_unit(
        &self,
        unit: &Frozen<PublishedUnit>,
    ) -> crate::unit_cache::EarlyBound {
        let mut published: Vec<Option<EpochClassId>> = vec![None; unit.classes.len()];
        let mut own: Vec<(Vec<u8>, EpochClassId)> = Vec::new();
        for (at, decl) in (0u32..).zip(&unit.classes) {
            let keyed = unit.runtime_keys.iter().any(|(_, class)| *class == at);
            if keyed || decl.flags.is_anonymous {
                continue;
            }
            let named = |name: &[u8]| {
                let engine = self.shared.class_at(name);
                engine
                    .and_then(|class| self.arena.shared_class(class))
                    .or_else(|| {
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
            let row: Declared<InArena> = Declared {
                unit: Frozen::clone(unit),
                at,
                composed: None,
                bound: Bound::default(),
            };
            let row = ClassRow::Arena(&row);
            let Some(record) = self.record_over(row, &|name| named(name).map(ClassId::from)) else {
                continue;
            };
            let Ok(class) = self.publish((unit, at), None, &record, parent, &|_| None) else {
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
            // The arena's one check mints the published classes'
            // identities, and none for the request's own class.
            let minted = |class: Option<ClassId>| class.and_then(|c| req.arena.shared_class(c));
            assert_eq!(minted(p).map(ClassId::from), p);
            assert_eq!(minted(c).map(ClassId::from), c);
            assert_eq!(minted(d), None, "D's number mints nothing");
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

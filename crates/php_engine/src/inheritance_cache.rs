//! php's inheritance cache (design §6.2; `Z/ext/opcache/ZendAccelerator.c:2274`):
//! the classes a link made as the program serves published, each found again by the
//! unlinked declaration it linked and by the published identities it was
//! linked against, so a request that links the same declaration against
//! the same classes declares the published class by its number and
//! links nothing.
//!
//! An entry is php's `zend_inheritance_cache_entry`: the parent, the
//! traits and the interfaces as the link fetched them, the variance
//! dependencies its checks read by name (`track_class_dependency`), each
//! with the class the name found, the published class, and what the link
//! raised, which a hit raises again (`zend_emit_recorded_errors_ex`). The
//! entries of one declaration are a chain, newest last, as php's
//! `ce->inheritance_cache` is a list on the unlinked `ce`.
//!
//! The map is engine-wide and lives in the arena of the epoch it
//! publishes into, so an opcache restart, which hands the next request a
//! new epoch, leaves it behind and it is dropped with the classes it
//! names once no request of its epoch runs (#2184, #2215). It is read on
//! every link the program makes and written once per
//! declaration and dependency set, so it is sharded by declaration, each
//! shard an `RwLock` that readers share: two requests linking at once
//! read side by side, and a publisher holds one shard of the 64 for one
//! append. A reader takes the chain's `Arc` and lets the shard go before
//! it checks a dependency, which can autoload and so serve PHP code that
//! links again; a publisher swaps the chain for a longer copy, so a chain a
//! reader holds never changes under it. This is the engine's
//! cross-request cache ADR-0005 allows; no extension reaches it.

use std::collections::HashMap;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, PoisonError, RwLock};

use php_link::Raised;
use php_unit::PublishedUnit;

use crate::api::values::ClassId;

/// How many shards the map is cut into.
const SHARDS: usize = 64;

/// One published link: what it was linked against and what it made.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The parent the link fetched, `entry->parent`.
    pub(crate) parent: Option<ClassId>,
    /// The traits, then the interfaces, the link fetched, in the order
    /// written, `entry->traits_and_interfaces`.
    pub(crate) uses: Box<[ClassId]>,
    /// Every other class a check read, by its name as written, beside
    /// the class the name found, `entry->dependencies`.
    pub(crate) dependencies: Box<[(Box<[u8]>, ClassId)]>,
    /// The published class, `entry->ce`.
    pub(crate) class: ClassId,
    /// What the link raised, `entry->warnings`.
    pub(crate) raised: Box<[Raised]>,
}

impl Entry {
    /// Whether `other` was linked against the same classes, which
    /// `zend_accel_inheritance_cache_add` finds before it adds a second.
    fn same_link(&self, other: &Entry) -> bool {
        self.parent == other.parent
            && self.uses == other.uses
            && self.dependencies == other.dependencies
    }
}

/// The entries of one unlinked declaration, newest last, beside the unit
/// that declares it, which the chain keeps alive so its address names it.
struct Chain {
    /// Held and never read: what keeps the key's address the unit's.
    _unit: PublishedUnit,
    entries: Arc<[Arc<Entry>]>,
}

/// One shard: chains by the declaring unit's address and the
/// declaration's index.
type Shard = HashMap<(usize, u32), Chain>;

/// The map ([its docs](self)).
pub(crate) struct InheritanceCache {
    shards: Box<[RwLock<Shard>]>,
    hasher: RandomState,
}

impl Default for InheritanceCache {
    fn default() -> InheritanceCache {
        InheritanceCache {
            shards: (0..SHARDS).map(|_| RwLock::default()).collect(),
            hasher: RandomState::new(),
        }
    }
}

/// The key of the declaration at `at` of `unit`: the unit's address,
/// which the chain's share of the unit keeps from being reused.
fn key(unit: &PublishedUnit, at: u32) -> (usize, u32) {
    (PublishedUnit::as_ptr(unit).addr(), at)
}

impl InheritanceCache {
    fn shard(&self, key: (usize, u32)) -> &RwLock<Shard> {
        let hash = self.hasher.hash_one(key);
        let at = usize::try_from(hash % SHARDS as u64).unwrap_or(0);
        // `at` is below the shard count, which the slice holds.
        &self.shards[at.min(SHARDS - 1)]
    }

    /// The entries published for the declaration at `at` of `unit`,
    /// newest last; none where it was never published.
    pub(crate) fn find(&self, unit: &PublishedUnit, at: u32) -> Option<Arc<[Arc<Entry>]>> {
        let key = key(unit, at);
        let shard = self
            .shard(key)
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        shard.get(&key).map(|chain| Arc::clone(&chain.entries))
    }

    /// `entry` added for the declaration at `at` of `unit`, or the entry
    /// another request added for the same link meanwhile, which is the
    /// one to declare.
    pub(crate) fn add(&self, unit: &PublishedUnit, at: u32, entry: Entry) -> Arc<Entry> {
        let key = key(unit, at);
        let mut shard = self
            .shard(key)
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let chain = shard.entry(key).or_insert_with(|| Chain {
            _unit: PublishedUnit::clone(unit),
            entries: Arc::new([]),
        });
        if let Some(found) = chain.entries.iter().find(|found| found.same_link(&entry)) {
            return Arc::clone(found);
        }
        let entry = Arc::new(entry);
        let mut entries = chain.entries.to_vec();
        entries.push(Arc::clone(&entry));
        chain.entries = entries.into();
        entry
    }

    /// How many declarations have a chain.
    #[cfg(test)]
    fn declarations(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.read().map_or(0, |shard| shard.len()))
            .sum()
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests {
    use std::sync::Arc;

    use php_unit::Unit;

    use super::{Entry, InheritanceCache};
    use crate::api::values::ClassId;

    fn entry(parent: u32, class: u32) -> Entry {
        Entry {
            parent: Some(ClassId(parent)),
            uses: Box::new([]),
            dependencies: Box::new([]),
            class: ClassId(class),
            raised: Box::new([]),
        }
    }

    #[test]
    fn a_link_is_found_by_its_declaration_and_added_once() {
        let cache = InheritanceCache::default();
        let unit = Unit::default().published(None);
        assert!(cache.find(&unit, 0).is_none());
        let first = cache.add(&unit, 0, entry(1, 10));
        let again = cache.add(&unit, 0, entry(1, 11));
        assert_eq!(again.class, first.class, "the same link is the first entry");
        cache.add(&unit, 0, entry(2, 12));
        let chain = cache.find(&unit, 0).unwrap_or_else(|| Arc::new([]));
        let classes: Vec<u32> = chain.iter().map(|entry| entry.class.0).collect();
        assert_eq!(classes, [10, 12]);
        assert!(cache.find(&unit, 1).is_none());
        assert_eq!(cache.declarations(), 1);
    }

    /// A restart hands the next request a new epoch whose map is empty,
    /// while a request of the old one keeps reading the old map.
    #[test]
    fn a_restart_empties_the_map() {
        let files = crate::unit_cache::Files::new(16);
        let epochs = crate::arena::Epochs::of(crate::arena::Arena::past(0, 0, files));
        let unit = Unit::default().published(None);
        let arena = epochs.enter(|_| false);
        arena.inheritance.add(&unit, 0, entry(1, 10));
        let restarted = epochs.enter(|_| true);
        assert!(restarted.inheritance.find(&unit, 0).is_none());
        assert!(
            arena.inheritance.find(&unit, 0).is_some(),
            "the old epoch's request reads its own"
        );
    }

    /// Readers on many threads while one writes: each sees a chain whole,
    /// as long as it was when read.
    #[test]
    fn readers_and_a_writer_share_the_map() {
        let cache = InheritanceCache::default();
        let unit = Unit::default().published(None);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..2000 {
                        let length = cache.find(&unit, 0).map_or(0, |chain| chain.len());
                        let classes = cache.find(&unit, 0).map_or_else(Vec::new, |chain| {
                            chain.iter().map(|entry| entry.class.0).collect()
                        });
                        assert!(classes.len() >= length);
                        assert!(classes.windows(2).all(|pair| pair[0] < pair[1]));
                    }
                });
            }
            for parent in 0..200 {
                cache.add(&unit, 0, entry(parent, 100 + parent));
            }
        });
        assert_eq!(cache.find(&unit, 0).map_or(0, |chain| chain.len()), 200);
    }
}

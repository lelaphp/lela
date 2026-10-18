//! Extension state as a request reaches it (ADR-0005): each bundled
//! extension's request state, built in bundle order as the request
//! starts and dropped in reverse, its engine state, and what the bundle
//! registered. The request states are [`RequestState`]'s, whose field no
//! file but this one reads.

use std::any::{Any, TypeId};
use std::hash::{Hash, Hasher};

use crate::api::abi::{ExtensionEntry, States};

use super::Request;

/// The bundle's slot table for its extensions' states: each state's
/// place among the request states, which are built in bundle order, and
/// among the engine states, by its type, settled once as the engine
/// starts. Reading a state is one index and one compare, as php reaches
/// an extension's per-thread struct through the id `ts_allocate_id`
/// gave it, not a scan of the bundle.
#[derive(Default)]
pub(crate) struct StateSlots {
    request: Slots,
    engine: Slots,
}

impl StateSlots {
    /// The places the bundle's `extensions` declare request states at,
    /// in bundle order, and the engine states `engine` holds.
    pub(crate) fn of(
        extensions: &[ExtensionEntry],
        engine: &[Box<dyn Any + Send + Sync>],
    ) -> StateSlots {
        let declared = extensions
            .iter()
            .filter_map(|extension| extension.request_state.as_ref())
            .map(|(type_id, _)| *type_id);
        StateSlots {
            request: Slots::of(declared),
            engine: Slots::of(engine.iter().map(|state| (**state).type_id())),
        }
    }
}

/// Types to their places by one multiply and one shift of the type's
/// id: a table the engine sizes and seeds as it starts so that no two
/// of the bundle's types share an entry (multiply-shift hashing), each
/// entry the type it holds and its place. A type the table does not
/// hold, one no extension declared, reads as none.
#[derive(Default)]
struct Slots {
    seed: u64,
    shift: u32,
    entries: Box<[Option<(TypeId, usize)>]>,
}

impl Slots {
    /// The table over `types`, each at its place among them, the first
    /// of a type twice.
    fn of(types: impl Iterator<Item = TypeId>) -> Slots {
        let mut held: Vec<(TypeId, usize)> = Vec::new();
        for (at, id) in types.enumerate() {
            if !held.iter().any(|(seen, _)| *seen == id) {
                held.push((id, at));
            }
        }
        // Four entries a type or more, so a seed that spreads them is met
        // within a few tries; each width tried with a run of odd seeds.
        let least = held.len().saturating_mul(4).next_power_of_two();
        for bits in least.trailing_zeros().max(2)..=20 {
            let shift = 64 - bits;
            for attempt in 0u64..256 {
                let seed = attempt.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
                let mut entries = vec![None; 1 << bits].into_boxed_slice();
                let spread = held.iter().all(|&(id, at)| {
                    entries
                        .get_mut(index(id, seed, shift))
                        .is_some_and(|entry| entry.replace((id, at)).is_none())
                });
                if spread {
                    return Slots {
                        seed,
                        shift,
                        entries,
                    };
                }
            }
        }
        Slots::default()
    }

    /// The place of `id`, none for a type the table does not hold.
    #[inline]
    fn get(&self, id: TypeId) -> Option<usize> {
        let (held, at) = (*self.entries.get(index(id, self.seed, self.shift))?)?;
        (held == id).then_some(at)
    }
}

/// The entry `id` falls in: its word times `seed`, the top bits.
#[inline]
fn index(id: TypeId, seed: u64, shift: u32) -> usize {
    let mut word = Word(0);
    id.hash(&mut word);
    usize::try_from(word.0.wrapping_mul(seed).checked_shr(shift).unwrap_or(0)).unwrap_or(0)
}

/// What a `TypeId` writes as its hash, the id being one already: a word
/// of its bits.
struct Word(u64);

impl Hasher for Word {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, word: u64) {
        self.0 ^= word;
    }
}

/// The extensions' request states, whose field no file but this one
/// reads.
pub(super) struct RequestState {
    states: States<(TypeId, Box<dyn Any + Send>)>,
}

impl Default for RequestState {
    fn default() -> RequestState {
        RequestState {
            states: States::new(),
        }
    }
}

impl Request {
    /// Each extension's request state, built in bundle order (ADR-0005)
    /// as the request is laid.
    pub(super) fn build_request_states(&mut self) {
        for extension in &self.shared.extensions {
            if let Some((type_id, build)) = &extension.request_state {
                self.state.states.push((*type_id, build()));
            }
        }
    }

    /// Every extension's request state dropped, in reverse bundle order,
    /// before a debug build's request leak sensor reads the heap, so a
    /// state that kept a block of the request's past `rshutdown` leaves
    /// it held. A state touched after this is built afresh.
    #[cfg(debug_assertions)]
    pub(super) fn drop_request_states(&mut self) {
        drop(std::mem::take(&mut self.state));
    }

    /// The request state of the calling extension, `T` being the type it
    /// declared with `request_state::<T>()` (ADR-0005), read at the slot
    /// the bundle gave it ([`StateSlots`]). A type no bundled extension
    /// declared, and a state touched after the request's states were
    /// dropped, is found by a scan and built on first touch, where the
    /// design names a bug.
    pub fn state<T: Default + Send + 'static>(&mut self) -> &mut T {
        let wanted = TypeId::of::<T>();
        let at = if let Some(at) = self.state_at(wanted) {
            at
        } else {
            self.state.states.push((wanted, Box::new(T::default())));
            self.state.states.len() - 1
        };
        // Found or held at `at` under `T`'s id, so a `T`; the `else` is
        // the type system's, not a path a request takes.
        let Some(state) =
            (self.state.states.get_mut(at)).and_then(|(_, slot)| slot.downcast_mut::<T>())
        else {
            return Box::leak(Box::new(T::default()));
        };
        state
    }

    /// The request state of type `T` where it stands, read through a
    /// request held shared: what [`Request::state`] answers once built,
    /// and `None` before a type no extension declared is first touched.
    #[must_use]
    pub(crate) fn state_ref<T: Send + 'static>(&self) -> Option<&T> {
        let at = self.state_at(TypeId::of::<T>())?;
        self.state.states.get(at)?.1.downcast_ref::<T>()
    }

    /// Where the request state of type `wanted` stands: at its slot, or
    /// where a scan finds a state built on first touch.
    fn state_at(&self, wanted: TypeId) -> Option<usize> {
        let states = &self.state.states;
        self.shared
            .state_slots
            .request
            .get(wanted)
            .filter(|&at| states.get(at).is_some_and(|(id, _)| *id == wanted))
            .or_else(|| states.iter().position(|(id, _)| *id == wanted))
    }

    /// The engine state of the calling extension, `E` being the type its
    /// `engine_state` built (ADR-0005). `None` for a type no extension
    /// built, where the design names a bug.
    #[must_use]
    pub fn engine<E: Send + Sync + 'static>(&self) -> Option<&E> {
        let at = self.shared.state_slots.engine.get(TypeId::of::<E>())?;
        self.shared.engine_states.get(at)?.downcast_ref::<E>()
    }

    /// Every value of type `T` an extension of the bundle handed over
    /// with [`ExtensionBuilder::register`], in bundle order: what
    /// `pdo_driver_hash` holds after every `MINIT` of the bundle has
    /// registered into it, immutable for the engine's life
    /// (ADR-0005).
    ///
    /// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
    pub fn registered<T: Send + Sync + 'static>(&self) -> impl Iterator<Item = &T> {
        self.shared
            .supplied
            .registered
            .under(TypeId::of::<T>())
            .filter_map(|(_, value)| value.downcast_ref::<T>())
    }
}

#[cfg(test)]
mod tests {
    use std::any::TypeId;

    use super::Slots;

    #[test]
    fn each_type_reads_its_place_and_a_type_never_declared_reads_none() {
        let types = [
            TypeId::of::<u8>(),
            TypeId::of::<String>(),
            TypeId::of::<u8>(),
            TypeId::of::<Vec<u32>>(),
            TypeId::of::<(i64, bool)>(),
        ];
        let slots = Slots::of(types.into_iter());
        assert_eq!(
            slots.get(TypeId::of::<u8>()),
            Some(0),
            "the first of a type twice"
        );
        assert_eq!(slots.get(TypeId::of::<String>()), Some(1));
        assert_eq!(slots.get(TypeId::of::<Vec<u32>>()), Some(3));
        assert_eq!(slots.get(TypeId::of::<(i64, bool)>()), Some(4));
        assert_eq!(slots.get(TypeId::of::<char>()), None);
        assert_eq!(Slots::of(std::iter::empty()).get(TypeId::of::<u8>()), None);
    }
}

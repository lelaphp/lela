//! Zvals kept each in a room of its own off the request's heap, where a
//! structure the engine keeps in a shape of its own holds a value php
//! keeps in a bucket of a hash table it already counts: the symbol
//! table's bucket a fetch by name for writing hands the opline after it
//! (`zend_fetch_var_address_helper`), whose bytes the table's data
//! already holds at php's size. A room's address stands from the moment
//! it is laid until the collection is dropped, whatever is laid or given
//! back meanwhile, so a [`ZvalRef`] to it outlives php's own bucket
//! pointer, which a resize moves.

use std::cell::Cell;
use std::ptr::NonNull;

use crate::zval::{Zval, ZvalRef};

/// The rooms, each a box of its own so the list growing moves none, and
/// the rooms given back, laid again before a new one is made. A room is
/// freed with the collection and never before, so a name of one kept
/// past its give-back reads `UNDEF`, as php's deleted bucket reads, and
/// never freed memory.
#[derive(Debug, Default)]
pub struct Places {
    #[expect(
        clippy::vec_box,
        reason = "each room is a box so the list growing moves no room a place names"
    )]
    rooms: Vec<Box<Cell<Zval>>>,
    free: Vec<ZvalRef>,
}

impl Places {
    /// `z` laid in a room, one given back first: the room's place, which
    /// owns what `z` owned.
    pub fn lay(&mut self, z: Zval) -> ZvalRef {
        if let Some(at) = self.free.pop() {
            at.write(z);
            return at;
        }
        // Named once the box is in the list: moving it there retags it,
        // and a name taken before would be a stranger to the box freed.
        self.rooms.push(Box::new(Cell::new(z)));
        let room: &Cell<Zval> = &self.rooms[self.rooms.len() - 1];
        ZvalRef::new(NonNull::from(room).cast())
    }

    /// The room `at` names given back, a place [`Places::lay`] answered:
    /// what it held is the caller's, and the room reads `UNDEF` until it
    /// is laid again.
    pub fn give_back(&mut self, at: ZvalRef) -> Zval {
        let held = at.read();
        at.write(Zval::UNDEF);
        self.free.push(at);
        held
    }
}

#[cfg(test)]
mod tests {
    use super::Places;
    use crate::zval::Zval;

    #[test]
    fn a_room_stands_where_it_was_laid_while_others_are_laid_and_given_back() {
        let mut places = Places::default();
        let first = places.lay(Zval::long(1));
        let rooms: Vec<_> = (0..64).map(|n| places.lay(Zval::long(n))).collect();
        assert_eq!(first.read().as_long(), Some(1));
        first.write(Zval::long(2));
        assert_eq!(places.give_back(first).as_long(), Some(2));
        assert!(first.read().is_undef());
        assert_eq!(places.lay(Zval::long(3)), first);
        assert_eq!(rooms[63].read().as_long(), Some(63));
    }
}

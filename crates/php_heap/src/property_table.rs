//! An object's property table as php's holds it (design §5.4), in the
//! one position space php's `arData` has: while the slots stand for the
//! declared properties ([`DeclaredRows::Slots`]) php's table opens with
//! one `IS_INDIRECT` per slot (`rebuild_object_properties`) and this
//! heap's holds the dynamic rows alone, so position `i` below the slot
//! count is slot `i` and every position past it is the table's own
//! bucket that many further on; once the table holds copies of them
//! ([`DeclaredRows::Copied`]) every position is the table's own.
//!
//! The internal pointer the six pointer natives move lives on the table
//! as php's `nInternalPointer` does, split the same way: among the
//! slots it is the table's lead, past them it is the table's own
//! pointer, which every write of the dynamic rows moves as php's moves
//! the pointer past the `IS_INDIRECT` buckets. A slot is a live bucket
//! to a move whether or not it holds a value, since php's `IS_INDIRECT`
//! is never a tombstone; `php_array_iter_seek_current` is what steps
//! over one nothing holds.

use crate::Heap;
use crate::array::{Arr, DeclaredRows, Key};
use crate::hooks::Fatal;
use crate::object::Obj;
use crate::zval::Zval;

/// What stands at one position of an object's property table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableRow {
    /// Declared slot `i`, php's `IS_INDIRECT` to `OBJ_PROP_NUM(obj, i)`,
    /// named by the key the engine mangles for it.
    Slot(u32),
    /// A bucket of the table under its key.
    Entry(Key),
}

/// The way `php_array_iter_seek_current` steps over a slot nothing
/// holds: on after `reset`, `next`, `current` and `key`, back after
/// `end` and `prev`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// `zend_hash_move_forward`.
    Forward,
    /// `zend_hash_move_backwards`.
    Backward,
}

/// An object and its property table, read and moved in php's positions.
/// A view: it holds no owner of either.
#[derive(Clone, Copy, Debug)]
pub struct PropertyTable {
    object: Obj,
    table: Arr,
}

impl Obj {
    /// The object's property table in php's positions, once something
    /// has laid it down.
    pub(crate) fn property_table(self) -> Option<PropertyTable> {
        self.properties().map(|table| PropertyTable {
            object: self,
            table,
        })
    }

    /// The object's property table as it stands, laid down by nothing:
    /// its own where something has laid it down, and otherwise its
    /// declared slots before the process's empty table, the positions
    /// php's encoders walk without rebuilding the table
    /// (`OBJ_PROP(obj, prop_info->offset)` over `default_properties_count`).
    #[must_use]
    pub fn slot_table(self) -> PropertyTable {
        PropertyTable {
            object: self,
            table: self.properties().unwrap_or_else(Arr::empty),
        }
    }
}

impl Heap {
    /// The object's property table in php's positions, laid down first
    /// where the object has none (`rebuild_object_properties`, through
    /// [`Heap::object_properties`]).
    pub fn object_property_table(&mut self, o: Obj) -> Result<PropertyTable, Fatal> {
        let table = self.object_properties(o)?;
        Ok(PropertyTable { object: o, table })
    }
}

impl PropertyTable {
    /// The table itself, `zobj->properties`.
    #[must_use]
    pub fn table(&self) -> Arr {
        self.table
    }

    /// The positions the declared slots take before the table's own
    /// buckets: one per slot while they stand for the declared rows,
    /// none once the table holds copies of them.
    #[must_use]
    pub fn declared(self) -> u32 {
        match self.table.declared_rows() {
            DeclaredRows::Slots => self.object.slots(),
            DeclaredRows::Copied => 0,
        }
    }

    /// `nNumUsed`: the positions written, tombstones included.
    #[must_use]
    pub fn used(&self) -> u32 {
        self.declared() + self.table.num_used()
    }

    /// `zend_hash_num_elements`: every declared row, held or not, and
    /// the table's own.
    #[must_use]
    pub fn len(&self) -> u32 {
        self.declared() + u32::try_from(self.table.len()).unwrap_or(u32::MAX)
    }

    /// Whether [`PropertyTable::len`] is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// What stands at `pos` and its value, `IS_INDIRECT` followed: the
    /// slot's zval, `UNDEF` where nothing holds it, or the bucket's;
    /// `None` for a tombstone and past the end. A view, no owner taken.
    #[must_use]
    pub fn row_at(&self, pos: u32) -> Option<(TableRow, Zval)> {
        let declared = self.declared();
        if pos < declared {
            return Some((TableRow::Slot(pos), self.object.slot(pos)));
        }
        self.table
            .entry_at(pos - declared)
            .map(|(key, z)| (TableRow::Entry(key), z))
    }

    /// Whether `pos` is a bucket to a move: every slot, and a bucket of
    /// the table's that is no tombstone.
    fn is_live(self, pos: u32) -> bool {
        let declared = self.declared();
        pos < declared || self.table.is_live(pos - declared)
    }

    /// `nInternalPointer` as it was written.
    #[must_use]
    pub(crate) fn raw_pointer(self) -> u32 {
        let declared = self.declared();
        let lead = self.table.lead();
        if lead < declared {
            lead
        } else {
            declared + self.table.raw_pointer()
        }
    }

    /// `nInternalPointer = pos`.
    pub(crate) fn set_pointer(self, pos: u32) {
        let declared = self.declared();
        if pos < declared {
            self.table.set_lead(pos);
        } else {
            self.table.set_lead(declared);
            self.table.set_raw_pointer(pos - declared);
        }
    }

    /// `_zend_hash_get_valid_pos`.
    fn valid_pos(self, mut pos: u32) -> u32 {
        let used = self.used();
        while pos < used && !self.is_live(pos) {
            pos += 1;
        }
        pos
    }

    /// `zend_hash_get_current_pos`: where the pointer stands, past any
    /// tombstone; [`PropertyTable::used`] past the end.
    #[must_use]
    pub(crate) fn position(self) -> u32 {
        self.valid_pos(self.raw_pointer())
    }

    /// `zend_hash_internal_pointer_reset`.
    pub fn reset(&self) {
        self.set_pointer(self.valid_pos(0));
    }

    /// `zend_hash_internal_pointer_end`: the last live position, or past
    /// the end where there is none.
    pub fn end(&self) {
        let used = self.used();
        let mut idx = used;
        while idx > 0 {
            idx -= 1;
            if self.is_live(idx) {
                self.set_pointer(idx);
                return;
            }
        }
        self.set_pointer(used);
    }

    /// `zend_hash_move_forward`: one live position on, or past the end;
    /// `false` where the pointer was past it already.
    pub fn forward(&self) -> bool {
        let used = self.used();
        let mut idx = self.position();
        if idx >= used {
            return false;
        }
        loop {
            idx += 1;
            if idx >= used || self.is_live(idx) {
                self.set_pointer(idx.min(used));
                return true;
            }
        }
    }

    /// `zend_hash_move_backwards`: one live position back, or past the
    /// end where there is none before it; `false` where the pointer was
    /// past the end already.
    pub fn backward(&self) -> bool {
        let used = self.used();
        let mut idx = self.raw_pointer();
        if idx >= used {
            return false;
        }
        while idx > 0 {
            idx -= 1;
            if self.is_live(idx) {
                self.set_pointer(idx);
                return true;
            }
        }
        self.set_pointer(used);
        true
    }

    /// `php_array_iter_seek_current` (`Z/ext/standard/array.c:1012`): the
    /// row at the pointer and its value, the pointer moved on `toward`
    /// over each slot nothing holds (an uninitialised typed property)
    /// and left where it stops; `None` past the end.
    pub fn seek(&self, toward: Direction) -> Option<(TableRow, Zval)> {
        loop {
            let (row, z) = self.row_at(self.position())?;
            if !z.is_undef() {
                return Some((row, z));
            }
            let moved = match toward {
                Direction::Forward => self.forward(),
                Direction::Backward => self.backward(),
            };
            if !moved {
                return None;
            }
        }
    }
}

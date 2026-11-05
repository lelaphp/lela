//! The owner the write after a `FETCH_DIM_W` takes of the table the
//! fetch walked to: the one hold of the engine that spans two oplines.
//! Every other is an [`Owner`] taken and given back around the call it
//! spans.

use php_heap::ZvalRef;

use super::Request;
use crate::api::values::{Owner, Value};

/// Where the last `FETCH_DIM_W`, or `FETCH_OBJ_RW` over a `VAR`, laid the
/// `IS_INDIRECT` of its `VAR`, and the array or object whose table or
/// slot the entry is in. What it names
/// is owned by whatever held the container when the fetch walked it,
/// and no PHP code may be reached between the fetch and the one read
/// of it: [`Request::hold_walk`] turns it into an owner or into
/// nothing, and the `ASSIGN_DIM`, `ASSIGN_REF` or step it is read by
/// stands next after the fetch that wrote it, as php's delayed fetch
/// compiles them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk {
    place: ZvalRef,
    table: Value,
}

/// The table the last `FETCH_DIM_W` laid its `IS_INDIRECT` into, whose fields no file but this one
/// reads.
#[derive(Default)]
pub(super) struct Holding {
    /// The table the last `FETCH_DIM_W` laid its `IS_INDIRECT` into,
    /// for the write after it to hold ([`Request::hold_walk`]).
    walk: Option<Walk>,
}

impl Request {
    /// What the `FETCH_DIM_W` just served laid down: the entry its
    /// `VAR` points at and the array or object that holds the entry, or
    /// nothing where the `VAR` holds no `IS_INDIRECT`. Every fetch
    /// notes one or the other, so what is noted is its own.
    pub(crate) fn note_walk(&mut self, walked: Option<(ZvalRef, Value)>) {
        self.hold.walk = walked.map(|(place, table)| Walk { place, table });
    }

    /// An owner of the table the `FETCH_DIM_W` before this opline walked
    /// to `place`, taken before PHP code can be reached: nothing where the
    /// fetch noted another place or none.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn hold_walk(&mut self, place: ZvalRef) -> Option<Owner<Value>> {
        let walk = self.hold.walk.take()?;
        (walk.place == place).then(|| Owner::take(self, walk.table))
    }

    /// The array or object the fetch before this opline walked to `place`.
    pub(crate) fn walked(&self, place: ZvalRef) -> Option<Value> {
        let walk = self.hold.walk.filter(|walk| walk.place == place)?;
        Some(walk.table)
    }
}

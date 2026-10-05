//! The owner a write takes of the table a dimension fetch walked to.
//! Every other owner the engine holds across a call that may reach PHP
//! code is an [`Owner`] taken where it is needed: php's `GC_ADDREF`
//! before a diagnostic, an accessor or a conversion that can reach a
//! user handler, and the `GC_DELREF` ([`Owner::let_go`]), `OBJ_RELEASE`
//! or `zend_string_release` ([`Owner::give_up`]) after it. A handler may
//! give up every other owner of the array a write is landing in, the
//! object an accessor is served on or the string a conversion is
//! reading; under the owner what it names stands until the owner is let
//! go, and what is left then says whether the handler freed it or took
//! an owner of its own, which is the test php's handler makes after the
//! call. This file keeps the one hold that spans two oplines.

use php_heap::ZvalRef;

use super::Request;
use crate::api::values::{Owner, Value};

/// Where the last `FETCH_DIM_W` laid the `IS_INDIRECT` of its `VAR`,
/// and the array or object whose table the entry is in. What it names
/// is owned by whatever held the container when the fetch walked it,
/// and no PHP code may be reached between the fetch and the one read
/// of it: [`Request::hold_walk`] turns it into an owner or into
/// nothing, and the `ASSIGN_DIM` it is read by stands next after the
/// fetch that wrote it, as php's delayed fetch compiles them.
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
    /// for the `ASSIGN_DIM` after it to hold ([`Request::hold_walk`]).
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
}

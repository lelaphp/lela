//! The owner the write after a `FETCH_DIM_W` takes of the table the
//! fetch walked to: the one hold of the engine that spans two oplines.
//! Every other is an [`Owner`] taken and given back around the call it
//! spans. Beside it, the row a fetch by name laid down, which no owner
//! keeps and a step past PHP code reads again where it stands
//! ([`Request::hold_named`]).

use php_heap::ZvalRef;

use super::Request;
use crate::api::values::{Owner, Value};
use crate::exec::TableId;

/// Where the last `FETCH_DIM_W`, `FETCH_OBJ_RW` over a `VAR` or fetch by
/// name laid the `IS_INDIRECT` of its `VAR`, and what the place is in.
/// What it names is owned by whatever held the container when the fetch
/// walked it, and no PHP code may be reached between the fetch and the
/// one read of it: [`Request::hold_walk`] turns it into an owner or into
/// nothing, [`Request::hold_named`] into the row a step reads again, and
/// the `ASSIGN`, `ASSIGN_DIM`, `ASSIGN_REF` or step it is read by stands
/// next after the fetch that wrote it, as php's delayed fetch compiles
/// them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk {
    place: ZvalRef,
    walked: Walked,
}

/// What a fetch found its place in.
#[derive(Debug, Clone, Copy)]
enum Walked {
    /// The array or object whose table or slot the entry is in.
    Table(Value),
    /// A symbol table, whose row a fetch by name laid down: one no owner
    /// keeps, which a write grows and an `unset` empties in place.
    Named(TableId),
}

/// The row a fetch by name laid down for a step that reaches PHP code
/// before it writes there, and where its table's rows stood as the step
/// began ([`Request::named_place`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NamedRow {
    place: ZvalRef,
    table: TableId,
    rows: usize,
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
        self.hold.walk = walked.map(|(place, table)| Walk {
            place,
            walked: Walked::Table(table),
        });
    }

    /// What a fetch by name just laid down: the place its `VAR` points
    /// at, in `table`, a row of it or the CV an attached row names.
    pub(crate) fn note_named(&mut self, place: ZvalRef, table: TableId) {
        self.hold.walk = Some(Walk {
            place,
            walked: Walked::Named(table),
        });
    }

    /// An owner of the table the `FETCH_DIM_W` before this opline walked
    /// to `place`, taken before PHP code can be reached: nothing where the
    /// fetch noted another place or none. A fetch by name's note stands,
    /// for [`Request::hold_named`].
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn hold_walk(&mut self, place: ZvalRef) -> Option<Owner<Value>> {
        let walk = self.hold.walk?;
        let Walked::Table(table) = walk.walked else {
            return None;
        };
        self.hold.walk = None;
        (walk.place == place).then(|| Owner::take(self, table))
    }

    /// The array or object the fetch before this opline walked to `place`.
    pub(crate) fn walked(&self, place: ZvalRef) -> Option<Value> {
        let walk = self.hold.walk.filter(|walk| walk.place == place)?;
        match walk.walked {
            Walked::Table(table) => Some(table),
            Walked::Named(_) => None,
        }
    }

    /// The row the fetch by name before this opline laid `place` down
    /// in, taken before PHP code can be reached: nothing where the fetch
    /// noted another place, or the place is a CV an attached row names,
    /// which stands as long as its frame does.
    pub(crate) fn hold_named(&mut self, place: ZvalRef) -> Option<NamedRow> {
        let walk = self.hold.walk?;
        let Walked::Named(table) = walk.walked else {
            return None;
        };
        self.hold.walk = None;
        let rows = self.symbol_arr(table)?;
        (walk.place == place && rows.holds_row(place)).then(|| NamedRow {
            place,
            table,
            rows: rows.rows_at(),
        })
    }

    /// `row`'s place where it stands past what ran since it was taken:
    /// none where a write grew its table, which moves every row, or an
    /// `unset` emptied it, which leaves php's bucket `UNDEF`. php writes
    /// through `variable_ptr` there all the same, into memory it freed
    /// or a bucket no lookup reaches, so the write is lost either way.
    pub(crate) fn named_place(&self, row: NamedRow) -> Option<ZvalRef> {
        let rows = self.symbol_arr(row.table)?;
        (rows.rows_at() == row.rows && !row.place.read().is_undef()).then_some(row.place)
    }
}

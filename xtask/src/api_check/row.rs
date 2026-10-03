//! One api-check reading as a row of `status --section api-check --format
//! jsonl --every` (harness-design.md §4.6, #1763), `locks` by its other
//! name, the section that renders the api-check readings: what the reading
//! counted, under a shape `v` versions. A seam's reading counts its lock's items and a crate's its
//! edges; which scope is a seam is the manifest's at the ref a reader
//! views, so a row says what one reading counted and leaves a count it does
//! not keep null, never 0.

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count};

/// The version of [`Row`]'s shape. It changes when the shape does, a
/// field added, removed, renamed or read differently, and never for a
/// new reading; a reader refuses a `v` it does not know.
pub(crate) const V: u32 = 1;

/// One api-check reading. The field order is the line's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    head: Head,
    /// A seam's lock lines: what `api_items` holds, `__private`'s apart
    /// where the seam's ceiling splits them; null for a crate's reading.
    items: Option<u64>,
    /// Of the seam's lines, those `__private` names, where its ceiling
    /// splits them.
    private: Option<u64>,
    /// A crate's workspace dependencies, each within its edges; null for a
    /// seam's reading.
    edges: Option<u64>,
    /// The extensions the bundle registers, on the bundle's reading.
    extensions: Option<u64>,
}

impl PerReading for Row {
    const INSTRUMENT: &'static str = INSTRUMENT;

    /// What one reading counted, each count under the key its writer keeps
    /// it by ([`numbers`]).
    fn of(reading: &Reading) -> Row {
        Row {
            head: Head::of(reading, V),
            items: count(reading, numbers::ITEMS),
            private: count(reading, numbers::PRIVATE),
            edges: count(reading, numbers::EDGES),
            extensions: count(reading, numbers::EXTENSIONS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Row;
    use crate::reading::Reading;
    use crate::status::{PerReading, every};

    /// A reading of `scope` with the counts named.
    fn reading(scope: &str, counts: &[(&str, i64)]) -> Reading {
        every::reading(super::INSTRUMENT, scope, counts)
    }

    /// A seam's reading counts its items and a crate's its edges; each
    /// leaves the other's null.
    #[test]
    fn a_seam_counts_items_and_a_crate_edges() {
        let api = Row::of(&reading("api", &[("items", 1045), ("private", 12)]));
        assert_eq!(
            (api.items, api.private, api.edges, api.extensions),
            (Some(1045), Some(12), None, None)
        );
        let bundle = Row::of(&reading("php_bundle", &[("edges", 59), ("extensions", 58)]));
        assert_eq!(
            (bundle.items, bundle.edges, bundle.extensions),
            (None, Some(59), Some(58))
        );
    }
}

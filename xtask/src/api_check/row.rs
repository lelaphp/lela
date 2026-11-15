//! One api-check reading as a row of `status --section api-check --format
//! jsonl --every` (harness-design.md §4.6), `locks` by its other
//! name, the section that renders the api-check readings: what the reading
//! counted, under a shape `v` versions. A seam's reading counts its lock's items and a crate's its
//! edges; which scope is a seam is the manifest's at the ref a reader
//! views, so a row says what one reading counted and leaves a count it does
//! not keep null, never 0.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{INSTRUMENT, numbers};
use crate::reading::Reading;
use crate::status::PerReading;
use crate::status::every::{Head, count, whole};

/// The version of [`Row`]'s shape, which changes only when the shape does
/// and never for a new reading; a reader refuses a `v` it does not know
/// (harness-design.md §4.6, revision 76). A field a row may omit, whose
/// absence a reader of the earlier shape already handles correctly, is
/// added under the same `v`; a field removed, renamed or retyped, or a
/// meaning changed, is a new `v`.
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
    /// An item lock's lines by the module that defines each, the
    /// growth of one module read across a series; left out where the
    /// reading keeps none, a crate's or one taken before api-check counted
    /// them, which a reader of the earlier shape already handles.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    modules: BTreeMap<String, u64>,
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
            modules: reading
                .groups
                .iter()
                .map(|(module, n)| (module.clone(), whole(*n)))
                .collect(),
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
        assert!(bundle.modules.is_empty());
    }

    /// A seam's per-module counts are its row's `modules`, and a row
    /// without them leaves the field out of its line.
    #[test]
    fn a_seam_carries_its_lines_per_module() {
        let mut reading = reading("api", &[("items", 3)]);
        reading.group("php_engine::api::request", 2).unwrap();
        reading.group("php_engine::api::values", 1).unwrap();
        let row = Row::of(&reading);
        let line = serde_json::to_value(&row).unwrap();
        assert_eq!(
            line["modules"],
            serde_json::json!({"php_engine::api::request": 2, "php_engine::api::values": 1})
        );
        let bare = serde_json::to_value(Row::of(&self::reading("api", &[]))).unwrap();
        assert!(bare.get("modules").is_none(), "{bare}");
    }
}

//! The port map's own reading of its two files: the census joined to the
//! lock as `data/port-map.v0.toml` carries it, and a census run as
//! `scripts/census.sh` prints it.

use std::path::Path;

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{Counted, PortMap, Ported, Source, Status, census, join, key, read};
use crate::api_check::lock::Lock;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A negative control of this instrument, under `xtask/fixtures/port-map`.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/port-map")
        .join(name)
}

/// A lock of these items.
fn lock(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|i| (*i).to_owned()).collect()
}

const ROWS: &str = r#"
# ---- mahout_value methods called from ext crates
[[item]]
mahout = "mahout_value::{PhpArray,PhpString}::len"
sites = 748
crates = 18
status = "same"
api = "values::PhpArray::len"

# ---- RequestCtx methods called from ext crates
[[item]]
mahout = "RequestCtx::note_alloc"
sites = 327
crates = 19
status = "gone"
rule = "nothing; the allocator accounts"

# ---- semantics:: items used from ext crates
[[item]]
mahout = "semantics::throw_internal"
sites = 181
crates = 17
status = "renamed"
api = "raising::throw_internal"
"#;

#[test]
fn a_row_carries_the_census_table_it_came_from_and_joins_on_the_name_the_census_prints() {
    let map = PortMap::parse(ROWS).unwrap();
    assert_eq!(map.rows.len(), 3);
    assert_eq!(
        map.rows[0].table,
        "mahout_value methods called from ext crates"
    );
    // The value methods are printed by name alone, so the qualifier the
    // map adds is not part of the key; every other table's prefix is
    // taken off.
    assert_eq!(map.rows[0].key(), "len");
    assert_eq!(map.rows[1].key(), "note_alloc");
    assert_eq!(map.rows[2].key(), "throw_internal");
    assert_eq!(map.rows[1].replacement(), "nothing; the allocator accounts");
    assert!(
        map.row("RequestCtx methods called from ext crates", "note_alloc")
            .is_some()
    );
    assert!(
        map.row("semantics:: items used from ext crates", "note_alloc")
            .is_none()
    );
}

#[test]
fn the_census_marks_the_ext_state_impl_and_neither_mark_is_the_key() {
    let table = "other mahout_runtime items imported or called from ext crates";
    assert_eq!(
        key(table, "mahout_runtime::ExtState::ext_teardown"),
        "ExtState::ext_teardown"
    );
    assert_eq!(
        key(table, "ExtState::ext_teardown (impl)"),
        "ExtState::ext_teardown"
    );
    // One row of that table is keyed on RequestCtx, which carries no
    // mahout_runtime prefix to take off.
    assert_eq!(
        key(table, "RequestCtx::ext_state_mut"),
        "RequestCtx::ext_state_mut"
    );
}

#[test]
fn a_row_outside_the_shape_is_refused_by_name() {
    let err = PortMap::parse(&ROWS.replace("status = \"gone\"", "status = \"dropped\""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("dropped"), "{err}");

    let err = PortMap::parse(&ROWS.replace("rule = \"nothing; the allocator accounts\"", ""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("carries no rule"), "{err}");

    let err = PortMap::parse(&ROWS.replace("api = \"values::PhpArray::len\"", ""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("names no api item"), "{err}");

    let err = PortMap::parse(&format!("{ROWS}{ROWS}"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("two rows under"), "{err}");

    let err = PortMap::parse(&ROWS.replace(
        "# ---- semantics:: items used from ext crates",
        "# ---- mahout_value tricks",
    ))
    .unwrap_err()
    .to_string();
    assert!(err.contains("no table"), "{err}");
}

/// The tables `scripts/census.sh` prints, as it prints them: the heading
/// with its count, the column header, the rows, the `~` on a name std
/// also defines, and the per-crate lists that are not table rows.
const PRINTED: &str = "\
# Mahout extension-seam census (scripts/census.sh) over /tmp/mahout
## mahout_value methods called from ext crates: 2 distinct items  (~ = std-colliding name)
 sites crates  item
   748     18  ~len
     3      1  sort_end

## mahout_linker: Cargo dependents 2, crates referencing it in source 2
## mahout_linker items imported or path-qualified by ext crates: 1 distinct items
 sites crates  item
    12      3  ClassDescriptor
  mahout_ext_spl: ClassDescriptor, ClassFlags
";

#[test]
fn a_census_run_reads_as_its_tables_rows_and_nothing_beside_them() {
    let rows = census(PRINTED);
    assert_eq!(rows.len(), 3, "{rows:#?}");
    assert_eq!(
        rows[0],
        Counted {
            table: "mahout_value methods called from ext crates".to_owned(),
            item: "len".to_owned(),
            sites: 748,
            crates: 18,
        }
    );
    assert_eq!(rows[2].item, "ClassDescriptor");
    assert_eq!(
        rows[2].table,
        "mahout_linker items imported or path-qualified by ext crates"
    );
    assert!(census("nothing here").is_empty());
}

#[test]
fn a_census_item_the_map_has_no_row_for_is_uncovered_and_a_row_it_no_longer_finds_is_stale() {
    let map = PortMap::parse(ROWS).unwrap();
    let lock = lock(&["values::PhpArray::len", "raising::throw_internal"]);
    let counted = census(PRINTED);
    let joined = join(
        &map,
        &lock,
        &Ported::default(),
        Source::Tree("/tmp/mahout".into()),
        &counted,
    );
    assert_eq!(joined.rows.len(), 1, "len is the one row the map covers");
    assert_eq!(joined.uncovered, ["sort_end", "ClassDescriptor"]);
    assert_eq!(joined.stale, 2);
    assert_eq!(
        joined.rows[0].sites, 748,
        "the census's count, not the map's"
    );
}

#[test]
fn the_committed_census_is_651_rows_whose_every_api_item_the_lock_carries() {
    let map = PortMap::load(workspace()).unwrap();
    let lock = Lock::read(&workspace().join("crates/php_ext/api.lock"))
        .unwrap()
        .lines;
    let joined = join(&map, &lock, &Ported::default(), Source::Seed, &[]);
    assert_eq!(joined.rows.len(), 651);
    assert_eq!(joined.rows_with(Status::Same), 165);
    assert_eq!(joined.rows_with(Status::Renamed), 336);
    assert_eq!(joined.rows_with(Status::Gone), 150);
    assert!(joined.unmatched().is_empty(), "{:?}", joined.unmatched());
    assert!(joined.uncovered.is_empty());
    assert_eq!(
        joined.unresolved(),
        651,
        "no extension is ported, so no row has evidence"
    );
}

#[test]
fn a_tree_without_crates_is_refused_and_nothing_is_joined() {
    let (source, counted) = read(workspace(), Some(Path::new("/tmp/no-such-mahout-tree"))).unwrap();
    assert!(counted.is_empty());
    let Source::Refused { why, .. } = &source else {
        panic!("{source:?}")
    };
    assert!(why.contains("no crates/"), "{why}");
}

/// The evidence is the extensions ported here, and there are none yet,
/// so no row of the committed census is resolved.
#[test]
fn no_extension_is_ported_yet_so_the_port_has_reached_no_row() {
    let manifest = crate::manifest::Manifest::load(workspace()).unwrap();
    let ported = Ported::read(workspace(), &manifest);
    assert!(ported.crates.is_empty(), "{:?}", ported.crates);
    assert!(!ported.reached(1));
}

/// A row is resolved when the port has reached as many extensions as the
/// census counted for it, so the rows one extension called go first and
/// the ones every extension called go last.
#[test]
fn the_port_reaching_one_extension_resolves_the_rows_one_extension_called() {
    let map = PortMap::parse(ROWS).unwrap();
    let lock = lock(&["values::PhpArray::len", "raising::throw_internal"]);
    let ported = Ported {
        crates: vec!["ext/standard".to_owned()],
    };
    let joined = join(&map, &lock, &ported, Source::Seed, &[]);
    // The three rows sat in 18, 19 and 17 extensions; one is not 17.
    assert_eq!(joined.unresolved(), 3);
    assert!(!ported.reached(2), "one extension is not two");
    assert!(ported.reached(1));
    assert!(
        !Ported::default().reached(0),
        "a row nobody called is no row"
    );

    let ported = Ported {
        crates: (0..19).map(|n| format!("ext/{n}")).collect(),
    };
    let joined = join(&map, &lock, &ported, Source::Seed, &[]);
    assert_eq!(joined.unresolved(), 0, "the port reached every row");
}

/// The negative control (harness-design.md §4.1): a row naming an `api`
/// item the lock does not carry is the design's unmatched row, and the
/// instrument fails on it, naming the item and no row that is in order.
#[test]
fn a_row_naming_an_api_item_the_lock_does_not_carry_fails_the_instrument() {
    let map = PortMap::parse(&std::fs::read_to_string(fixture("unmatched/port-map.toml")).unwrap())
        .unwrap();
    let lock = Lock::read(&fixture("unmatched/api.lock")).unwrap().lines;
    let joined = join(&map, &lock, &Ported::default(), Source::Seed, &[]);
    assert_eq!(joined.unmatched(), ["raising::engine_limitation"]);

    let manifest = crate::manifest::Manifest::load(workspace()).unwrap();
    let krate = super::home_of_the_lock(&manifest).unwrap();
    let reading = super::reading(&joined, &Ported::default(), krate, workspace()).unwrap();
    assert_eq!(reading.status, crate::reading::Status::Fail, "{reading:?}");
    assert_eq!(reading.numbers["unmatched"], 1.into());
    assert!(
        reading.message.contains("raising::engine_limitation")
            && reading.message.contains("does not carry"),
        "the offending item is named: {}",
        reading.message
    );
    assert!(
        !reading.message.contains("values::PhpArray::len"),
        "a row in order is not named: {}",
        reading.message
    );
}

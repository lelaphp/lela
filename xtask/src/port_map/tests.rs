//! The port map's own reading of its two files: the census joined to the
//! lock as `data/port-map.v0.toml` carries it, and a census run as
//! `scripts/census.sh` prints it.

use std::path::Path;

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{
    Counted, PortMap, Ported, Source, Status, census, join, key, read, registers,
    registers_a_native,
};
use crate::api_check::lock::Lock;

fn workspace() -> &'static Path {
    crate::package_dir().parent().unwrap()
}

/// A negative control of this instrument, under `xtask/fixtures/port-map`.
fn fixture(name: &str) -> PathBuf {
    crate::package_dir().join("fixtures/port-map").join(name)
}

/// A lock of these items.
fn lock(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|i| (*i).to_owned()).collect()
}

/// A port that has reached these extensions and no others.
fn ported(names: &[&str]) -> Ported {
    Ported {
        crates: names.iter().map(|n| format!("ext/{n}")).collect(),
    }
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

# ---- mahout_value free functions called from ext crates
[[item]]
mahout = "mahout_value::convert::float_to_repr"
sites = 3
crates = 2
status = "renamed"
api = "values::float_to_repr"

# ---- mahout_native_abi items used from ext crates
[[item]]
mahout = "mahout_native_abi::InOut::into_parts"
sites = 14
crates = 1
status = "gone"
rule = "an out parameter is a Ref"

# ---- mahout_native_macros attributes and their arguments
[[item]]
mahout = "mahout_native_macros::php_method(sig)"
sites = 428
crates = 7
status = "renamed"
api = "macros::php_method"
"#;

#[test]
fn a_row_carries_the_census_table_it_came_from_and_joins_on_the_name_the_census_prints() {
    let map = PortMap::parse(ROWS).unwrap();
    assert_eq!(map.rows.len(), 6);
    assert_eq!(
        map.rows[0].table,
        "mahout_value methods called from ext crates"
    );
    // The value methods are printed by name alone, so the qualifier the
    // map adds is not part of the key; a table printed by qualified name
    // has its prefix taken off.
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

/// Three tables print a name the seed's qualifier does not reach by a
/// prefix alone: the free functions bare where the seed keeps the
/// `convert::` path, the abi methods bare where the seed keeps the type,
/// the macros as an attribute where the seed keeps the crate. Each row's
/// key is what the census prints, and the census's own spelling keys to
/// itself, so a regenerated census at the pin joins all 651.
#[test]
fn the_three_tables_the_seed_spells_apart_join_on_the_name_the_census_prints() {
    let map = PortMap::parse(ROWS).unwrap();
    let printed = census(PRINTED);
    for (row, item) in [
        (&map.rows[3], "float_to_repr"),
        (&map.rows[4], "into_parts"),
        (&map.rows[5], "#[php_method(sig)]"),
    ] {
        assert_eq!(row.key(), item, "{}", row.mahout);
        assert_eq!(
            key(&row.table, item),
            item,
            "the census's spelling is the key"
        );
        let counted = printed
            .iter()
            .find(|c| c.table == row.table)
            .unwrap_or_else(|| panic!("PRINTED carries no row under {}", row.table));
        assert_eq!(counted.item, item);
        assert!(
            map.row(&counted.table, &key(&counted.table, &counted.item))
                .is_some()
        );
    }
    let macros = "mahout_native_macros attributes and their arguments";
    assert_eq!(
        key(macros, "mahout_native_macros::php_class"),
        "#[php_class]"
    );
    let abi = "mahout_native_abi items used from ext crates";
    assert_eq!(key(abi, "mahout_native_abi::Str"), "Str");
    assert_eq!(key(abi, "mahout_native_abi::NativeError::new"), "new");
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

/// The names and the count are one column: a row that names its
/// extensions names as many as it counts, each once. A row that names
/// none is the committed census's shape and is held to nothing.
#[test]
fn a_row_naming_its_extensions_names_as_many_as_it_counts() {
    let named = ROWS.replace(
        "mahout = \"semantics::throw_internal\"\nsites = 181\ncrates = 17",
        "mahout = \"semantics::throw_internal\"\nsites = 181\ncrates = 2\next = [\"standard\", \"spl\"]",
    );
    assert_eq!(
        PortMap::parse(&named).unwrap().rows[2].ext,
        ["standard", "spl"]
    );

    let err = PortMap::parse(&named.replace("crates = 2", "crates = 3"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("counts 3 crates and names 2"), "{err}");

    let err = PortMap::parse(&named.replace("\"standard\", \"spl\"", "\"spl\", \"spl\""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("names an extension twice"), "{err}");

    // The seed's rows carry the count alone, and that is in order.
    assert!(
        PortMap::parse(ROWS)
            .unwrap()
            .rows
            .iter()
            .all(|r| r.ext.is_empty())
    );
}

/// The tables `scripts/census.sh` prints, as it prints them: the heading
/// with its count, the column header, the rows, each with the `ext:` line
/// under it naming the extensions it counts, the `~` on a name std also
/// defines, an item carrying spaces, and the per-crate lists that are
/// not table rows.
const PRINTED: &str = "\
# Mahout extension-seam census (scripts/census.sh) over /tmp/mahout
## mahout_value methods called from ext crates: 2 distinct items  (~ = std-colliding name)
 sites crates  item
   748     18  ~len
               ext: ctype curl date dom filter iconv json mbstring openssl pcre pdo pdo_mysql pdo_sqlite random reflection session spl standard
     3      1  sort_end
               ext: standard

## mahout_value free functions called from ext crates: 1 distinct items
 sites crates  item
     3      2  float_to_repr
               ext: json standard

## mahout_native_abi items used from ext crates: 1 distinct items
 sites crates  item
    14      1  into_parts
               ext: pcre

## mahout_native_macros attributes and their arguments: 1 distinct items
 sites crates  item
   428      7  #[php_method(sig)]
               ext: date dom pdo pdo_sqlite reflection session spl

## mahout_linker: Cargo dependents 2, crates referencing it in source 2
## mahout_linker items imported or path-qualified by ext crates: 1 distinct items
 sites crates  item
    12      3  ClassDescriptor
               ext: date reflection spl
  mahout_ext_spl: ClassDescriptor, ClassFlags

## other mahout_runtime items imported or called from ext crates (Reentrant, ExtState, ...): 1 distinct items
 sites crates  item
     2      2  ExtState::ext_teardown (impl)
               ext: session spl
";

#[test]
fn a_census_run_reads_as_its_tables_rows_and_nothing_beside_them() {
    let rows = census(PRINTED);
    assert_eq!(rows.len(), 7, "{rows:#?}");
    assert_eq!(
        rows[0],
        Counted {
            table: "mahout_value methods called from ext crates".to_owned(),
            item: "len".to_owned(),
            sites: 748,
            crates: 18,
            ext: [
                "ctype",
                "curl",
                "date",
                "dom",
                "filter",
                "iconv",
                "json",
                "mbstring",
                "openssl",
                "pcre",
                "pdo",
                "pdo_mysql",
                "pdo_sqlite",
                "random",
                "reflection",
                "session",
                "spl",
                "standard",
            ]
            .map(str::to_owned)
            .to_vec(),
        }
    );
    assert_eq!(rows[1].ext, ["standard"]);
    assert_eq!(rows[2].item, "float_to_repr");
    assert_eq!(rows[3].item, "into_parts");
    assert_eq!(rows[4].item, "#[php_method(sig)]");
    assert_eq!(rows[5].item, "ClassDescriptor");
    assert_eq!(
        rows[5].table,
        "mahout_linker items imported or path-qualified by ext crates"
    );
    assert_eq!(rows[5].ext, ["date", "reflection", "spl"]);
    // The names go under the row whose item carries spaces as under any
    // other, since they are the line beneath it and not a column of it.
    assert_eq!(rows[6].item, "ExtState::ext_teardown (impl)");
    assert_eq!(rows[6].ext, ["session", "spl"]);
    assert!(census("nothing here").is_empty());
}

/// A census printed without the `ext:` lines, as the census printed
/// before the column, reads as rows naming no extension: a run of the
/// older shape is unresolved on every row and not resolved on the map's.
#[test]
fn a_census_run_without_the_ext_lines_names_no_extension() {
    let unnamed = PRINTED
        .lines()
        .filter(|line| !line.trim_start().starts_with("ext:"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = census(&unnamed);
    assert_eq!(rows.len(), 7);
    assert!(rows.iter().all(|row| row.ext.is_empty()), "{rows:#?}");
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
    assert_eq!(
        joined.rows.len(),
        4,
        "len and the three rows the seed spells apart are what the map covers"
    );
    assert_eq!(
        joined.uncovered,
        [
            "sort_end",
            "ClassDescriptor",
            "ExtState::ext_teardown (impl)"
        ]
    );
    assert_eq!(joined.stale, 2, "note_alloc and throw_internal");
    assert_eq!(
        joined.rows[0].sites, 748,
        "the census's count, not the map's"
    );
}

/// Under `--tree` a row resolves on the extensions the census just
/// printed and not on the map row's column (design §4.4): the seed names
/// none, and a regenerated census that names `standard` for a row takes
/// it off the checklist once `standard` is ported, however the seed's
/// column stands; and the map's column, when it disagrees, is not read.
#[test]
fn under_a_tree_a_row_resolves_on_the_census_names_and_not_the_map_rows() {
    let map = PortMap::parse(ROWS).unwrap();
    let lock = lock(&["values::PhpArray::len", "raising::throw_internal"]);
    let counted = census(
        "\
## mahout_value methods called from ext crates: 1 distinct items
 sites crates  item
   748      1  ~len
               ext: standard
",
    );
    let tree = || Source::Tree("/tmp/mahout".into());

    let reached = join(&map, &lock, &ported(&["standard"]), tree(), &counted);
    assert_eq!(reached.rows.len(), 1);
    assert!(reached.rows[0].named, "the census named it");
    assert!(
        reached.rows[0].resolved,
        "the seed's row names none, and the census's name is the one read"
    );
    assert_eq!(reached.rows[0].ext, ["standard"]);
    assert_eq!(
        reached.rows[0].crates, 1,
        "the census's count, not the map's"
    );
    assert_eq!(reached.unresolved(), 0);
    assert_eq!(reached.unnamed(), 0);

    let unreached = join(&map, &lock, &ported(&["spl"]), tree(), &counted);
    assert!(!unreached.rows[0].resolved, "standard is not ported");

    // A map row that names its extensions is not what the tree reads
    // either: the seed says spl, the census says standard, and the row
    // resolves on standard.
    let named = PortMap::parse(&ROWS.replace(
        "sites = 748\ncrates = 18",
        "sites = 748\ncrates = 1\next = [\"spl\"]",
    ))
    .unwrap();
    let on_the_census = join(&named, &lock, &ported(&["standard"]), tree(), &counted);
    assert!(on_the_census.rows[0].resolved);
    let on_the_map = join(&named, &lock, &ported(&["spl"]), tree(), &counted);
    assert!(!on_the_map.rows[0].resolved);

    // The committed census is read from the map's column, which is what
    // it has.
    let seed = join(&named, &lock, &ported(&["spl"]), Source::Seed, &[]);
    assert!(seed.rows[0].resolved);
    assert_eq!(seed.rows[0].ext, ["spl"]);
}

/// The committed census joined to the lock, held to what stays true as
/// either grows: every row is one of the three statuses and its
/// `api` item is one the lock carries, and the committed census names no
/// extension on any row (design §4.4), so with none ported every row is
/// unresolved. A row added, a status rewritten or a lock line added moves
/// no assertion here; a row whose item the lock lost is this test red.
#[test]
fn the_committed_census_joins_every_row_to_an_item_the_lock_carries() {
    let map = PortMap::load(workspace()).unwrap();
    let lock = Lock::read(&workspace().join("crates/php_ext/api.lock"))
        .unwrap()
        .lines;
    let joined = join(&map, &lock, &Ported::default(), Source::Seed, &[]);
    let rows = joined.rows.len();
    assert_eq!(rows, map.rows.len(), "every row is joined");
    assert!(rows > 0);
    assert_eq!(
        joined.rows_with(Status::Same)
            + joined.rows_with(Status::Renamed)
            + joined.rows_with(Status::Gone),
        rows
    );
    assert!(joined.unmatched().is_empty(), "{:?}", joined.unmatched());
    assert!(joined.uncovered.is_empty());
    assert_eq!(
        joined.unresolved(),
        rows,
        "no extension is ported, so no row has evidence"
    );
    assert_eq!(
        joined.unnamed(),
        rows,
        "the committed census carries the count alone (design §4.4)"
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

/// The evidence is which extensions are ported here, and the premise
/// held is one no commit ends: `Ported::read` names exactly the
/// laid-down crates whose sources take a registering step of the
/// builder, and a crate `new-ext` laid down, whose `extension()`
/// declares its state and registers nothing, is not evidence however
/// many stand beside the ported ones. `ext/standard` is among the ported
/// since `str_repeat`; which others are is the workspace's to
/// say, not this test's. The committed census names
/// no extension on any row, so no row is resolved by any of them yet.
#[test]
fn ported_names_exactly_the_laid_down_crates_that_register_a_native() {
    let manifest = crate::manifest::Manifest::load(workspace()).unwrap();
    let laid: Vec<&crate::manifest::Crate> = manifest
        .crates
        .iter()
        .filter(|krate| krate.name.starts_with("ext/"))
        .filter(|krate| workspace().join(&krate.name).join("Cargo.toml").is_file())
        .collect();
    assert!(!laid.is_empty(), "the crates are laid down: {laid:?}");
    let registering: Vec<String> = laid
        .iter()
        .filter(|krate| registers_a_native(workspace(), krate))
        .map(|krate| krate.name.clone())
        .collect();
    let here = Ported::read(workspace(), &manifest);
    assert_eq!(here.crates, registering, "{:?}", here.crates);
    assert!(here.carries("standard"), "{:?}", here.crates);
    assert!(
        registering.len() < laid.len(),
        "some laid-down crate registers nothing yet: {registering:?}"
    );
    assert!(ported(&["standard"]).carries("standard"));
    assert!(!ported(&["standard"]).carries("spl"));
}

/// What tells a ported extension from one `new-ext` laid down (design
/// §4.4): the crate root the template writes registers no native, and
/// one registering step of the builder makes it register one.
#[test]
fn the_template_registers_no_native_and_one_builder_step_registers_one() {
    let template = crate::new_ext::lib_rs(
        &crate::manifest::Manifest::named("ext/standard"),
        "standard",
    );
    assert!(!registers(&template), "the template registers nothing");
    assert!(registers(&template.replace(
        ".request_state::<State>()",
        ".request_state::<State>()\n        .functions(&[strlen::DECL])"
    )));
    assert!(
        !registers("/// `.functions(..)` registers a native.\nfn prose() {}\n"),
        "the step is a call on the builder; the word in prose is not one"
    );
}

/// The measure of *resolved* (design §4.4), against the fixture that
/// holds it: a row every one of whose extensions is ported is off the
/// checklist, a row one of whose is not stays on it, and a row that names
/// none stays on it whatever the port has reached.
#[test]
fn a_row_is_resolved_when_every_extension_the_census_counted_for_it_is_ported() {
    let map = PortMap::parse(&std::fs::read_to_string(fixture("resolved/port-map.toml")).unwrap())
        .unwrap();
    let lock = Lock::read(&fixture("resolved/api.lock")).unwrap().lines;

    let reached_two = join(
        &map,
        &lock,
        &ported(&["standard", "random"]),
        Source::Seed,
        &[],
    );
    assert!(reached_two.unmatched().is_empty(), "the fixture is matched");
    assert!(reached_two.rows[0].resolved, "both of its two are ported");
    assert!(!reached_two.rows[1].resolved, "spl is not ported");
    assert!(!reached_two.rows[2].resolved, "it names no extension");
    assert_eq!(reached_two.unresolved(), 2);
    assert_eq!(reached_two.unnamed(), 1);

    let reached_none = join(&map, &lock, &Ported::default(), Source::Seed, &[]);
    assert_eq!(
        reached_none.unresolved(),
        3,
        "an unported engine resolves nothing"
    );

    let reached_all = join(
        &map,
        &lock,
        &ported(&["standard", "random", "spl"]),
        Source::Seed,
        &[],
    );
    assert_eq!(
        reached_all.unresolved(),
        1,
        "the row that names no extension is not resolved by the empty list"
    );
    assert_eq!(reached_all.unnamed(), 1);

    // The port reaching an extension no row names moves nothing.
    let elsewhere = join(&map, &lock, &ported(&["json", "hash"]), Source::Seed, &[]);
    assert_eq!(elsewhere.unresolved(), 3);
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
    let reading = super::reading(
        &joined,
        &Ported::default(),
        krate,
        &crate::key::Snapshot::disk(workspace()).unwrap(),
    )
    .unwrap();
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

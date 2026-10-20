//! `xtask api-check` over the fixtures of `xtask/fixtures/api-check`, judged
//! by the tracked manifest: locks, edges and bundle order (ticket #4). One
//! fixture per rule the instrument evaluates must fail; `clean` must pass.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use xtask::api_check;
use xtask::key::Snapshot;
use xtask::manifest::{LockCeiling, Manifest, OneOrMany, Rule, Seam, Section};
use xtask::reading::{Number, Reading, Status};
use xtask::scratch::Tree;

mod common {
    pub mod repo;
}
use common::repo::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture(name: &str) -> PathBuf {
    xtask::package_dir().join("fixtures/api-check").join(name)
}

fn readings(name: &str) -> Vec<Reading> {
    let manifest = Manifest::load(workspace()).unwrap();
    api_check::hold(&manifest, &fixture(name), false).unwrap()
}

fn failures(readings: &[Reading]) -> Vec<&Reading> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .collect()
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of scope {scope}"))
}

#[test]
fn the_clean_fixture_passes_every_rule() {
    let readings = readings("clean");
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));

    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Pass, "{}", heap.message);
    assert_eq!(heap.numbers["items"], 9.into());
    let api = of(&readings, "api");
    assert_eq!(api.status, Status::Pass, "{}", api.message);
    assert_eq!(api.numbers["items"], 8.into(), "re-exports are followed");
    assert_eq!(api.adr, "0002, 0004, 0005");
    assert_eq!(api.rule, "api.lock-line");
    assert_eq!(of(&readings, "ext/random").rule, "ext.pub-beyond-extension");
    assert!(
        of(&readings, "ext/random")
            .message
            .contains("1 pub items of ext/random beyond extension() match ext/random/surface.lock"),
        "the pub surface beyond extension() is held to its surface.lock"
    );
    assert!(
        of(&readings, "ext/standard")
            .message
            .contains("1 link to an extension stands on a row of design §4.10's list of surfaces"),
        "ext/standard's link to ext/random is on the fixture's row"
    );
    assert_eq!(of(&readings, "sapi").status, Status::Pass);
    assert_eq!(
        of(&readings, "unit").status,
        Status::Skip,
        "no php_unit here"
    );
    assert_eq!(of(&readings, "php_bundle").status, Status::Pass);
    assert_eq!(of(&readings, "ext/standard").status, Status::Pass);
    for reading in &readings {
        assert_eq!(reading.instrument, "api-check");
        assert_eq!(reading.key().len(), 40, "{}", reading.scope);
    }
}

/// A `pub use` of a name a crate holds as a private module and a `pub fn`
/// re-exports the function, rustc keeping the two in two namespaces
/// (#2791): the `namespace` fixture's `php_ext` re-exports
/// `php_macros::marker` with no line for it in `api.lock`, which lists
/// the private module's item instead, the lock hole the inventory read
/// before, when it took the name for the module.
#[test]
fn a_reexported_fn_beside_a_module_of_its_name_wants_its_own_line() {
    let readings = readings("namespace");
    let api = of(&readings, "api");
    assert_eq!(api.status, Status::Fail, "{}", api.message);
    assert_eq!(
        api.message,
        "1 public items of php_ext have no line in crates/php_ext/api.lock: marker; \
         1 lines of crates/php_ext/api.lock name no public item: marker::inner"
    );
}

/// An `ext/*` crate's library dependency on another that no row of
/// design §4.10's list of surfaces names with both crates fails the
/// consumer's reading (#2251): the `surface-edge` fixture's ext/zip links
/// ext/pcre, which the one row, `pcre` → `spl`, does not name, while
/// ext/spl's link stands on it and ext/zip's dev-dependency on ext/spl
/// links nothing and is not held.
#[test]
fn an_extension_link_on_no_row_of_the_list_of_surfaces_fails() {
    let readings = readings("surface-edge");
    let zip = of(&readings, "ext/zip");
    assert_eq!(zip.status, Status::Fail, "{}", zip.message);
    assert_eq!(zip.rule, "ext.pub-beyond-extension");
    assert!(
        zip.message.starts_with(
            "ext/zip links ext/pcre, and no row of design §4.10's list of surfaces names `pcre` → `zip`"
        ),
        "{}",
        zip.message
    );
    assert!(!zip.message.contains("ext/spl"), "{}", zip.message);
    let spl = of(&readings, "ext/spl");
    assert_eq!(spl.status, Status::Pass, "{}", spl.message);
    assert_eq!(of(&readings, "ext/pcre").status, Status::Pass);
}

/// An extension's `pub` items beyond `extension()` are its
/// `surface.lock`, diffed as an item lock is (#2251): the `surface-lock`
/// fixture's lock lists `Regex` and `Regex::compile` and a line naming
/// nothing, and not `Regex::study` or `unlisted`.
#[test]
fn a_surface_item_without_its_surface_lock_line_fails() {
    let readings = readings("surface-lock");
    let pcre = of(&readings, "ext/pcre");
    assert_eq!(pcre.status, Status::Fail, "{}", pcre.message);
    assert_eq!(pcre.rule, "ext.pub-beyond-extension");
    assert_eq!(
        pcre.message,
        "2 pub items of ext/pcre beyond extension() have no line in ext/pcre/surface.lock: \
         Regex::study, unlisted; 1 lines of ext/pcre/surface.lock name no public item: gone"
    );
}

/// A surface item's `#[mirrors]` is held against the pinned php-src
/// (#2353): the `mirrors` fixture's tree stands in for it, and of its three
/// markers the one naming `php_random_default_algo`, which the header
/// declares `PHPAPI`, holds, while the one naming the header's `static
/// inline` and the one naming a header the tree lacks fail the reading.
#[test]
fn a_marker_naming_what_the_pinned_header_does_not_declare_fails() {
    let mut manifest = Manifest::load(workspace()).unwrap();
    let root = fixture("mirrors");
    manifest.pin = manifest.pin.on_this_machine(Some(root.join("php-src")));
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    let random = of(&readings, "ext/random");
    assert_eq!(random.status, Status::Fail, "{}", random.message);
    assert_eq!(random.rule, "ext.pub-beyond-extension");
    let tag = &manifest.pin.source.tag;
    assert_eq!(
        random.message,
        format!(
            "src/lib.rs's `Rng::engine` mirrors `php_random_default_engine`, which \
             ext/random/php_random.h at {tag} declares with none of PHPAPI, PDO_API, ZEND_API; \
             src/lib.rs's `absent` mirrors ext/random/php_rng.h, which php-src at {tag} lacks; \
             4 pub items of ext/random beyond extension() match ext/random/surface.lock"
        )
    );
}

/// `--write` writes an extension's `surface.lock` with a header before the
/// run's snapshot, so the reading taken after it passes over what it wrote
/// and is stamped over it.
#[test]
fn write_lays_down_a_surface_lock_the_reading_then_holds() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = Tree::new("api-check-surface-write").unwrap();
    xtask::scratch::repository(&fixture("surface-lock"), &root, &["."]).unwrap();
    let lock = root.join("ext/pcre/surface.lock");
    fs::remove_file(&lock).unwrap();
    let readings = api_check::hold(&manifest, &root, true).unwrap();
    let pcre = of(&readings, "ext/pcre");
    assert_eq!(pcre.status, Status::Pass, "{}", pcre.message);
    let home = manifest.scope_path("ext/pcre").unwrap();
    assert_eq!(
        pcre.key(),
        Snapshot::disk(&root).unwrap().key(&home).unwrap()
    );
    let written = fs::read_to_string(&lock).unwrap();
    assert!(
        written.starts_with("# The pub items of ext/pcre beyond extension()"),
        "{written}"
    );
    assert!(
        written.ends_with("\nRegex\nRegex::compile\nRegex::study\nunlisted\n"),
        "{written}"
    );
}

/// An item lock's reading counts its lines by the module that defines
/// each, beside the total (#1795): a re-export by the module it is written
/// in, `Severity` by `raising`'s, and a method by its impl block's. The
/// counts sum to the total, and an import lock and a crate keep none.
#[test]
fn an_item_lock_counts_its_lines_by_the_module_that_defines_each() {
    let readings = readings("clean");
    let groups = |scope: &str| &of(&readings, scope).groups;
    let counted = |pairs: &[(&str, i64)]| -> BTreeMap<String, Number> {
        pairs
            .iter()
            .map(|(module, n)| ((*module).to_owned(), (*n).into()))
            .collect()
    };
    assert_eq!(
        *groups("api"),
        counted(&[("php_engine::api::abi", 5), ("php_engine::api::raising", 3)])
    );
    assert_eq!(
        *groups("heap"),
        counted(&[
            ("php_heap", 2),
            ("php_heap::hidden", 2),
            ("php_heap::stack", 1),
            ("php_heap::stack::entry", 4),
        ])
    );
    for scope in ["api", "heap"] {
        let sum: i64 = groups(scope)
            .values()
            .map(|n| match n {
                Number::Int(n) => *n,
                Number::Float(n) => panic!("{scope}: a count of {n}"),
            })
            .sum();
        assert_eq!(of(&readings, scope).numbers["items"], sum.into(), "{scope}");
    }
    assert!(groups("sapi").is_empty(), "an import lock counts imports");
    assert!(groups("php_bundle").is_empty(), "a crate keeps none");
}

#[test]
fn a_public_item_without_a_lock_line_fails_naming_the_lock_file() {
    let readings = readings("lock-line");
    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Fail);
    assert!(
        heap.message.contains("crates/php_heap/heap.lock"),
        "{}",
        heap.message
    );
    assert!(heap.message.contains("alloc"), "{}", heap.message);
    assert_eq!(heap.adr, "0003");
    assert_eq!(failures(&readings).len(), 1);
}

/// A type `include!`d from `OUT_DIR`, as `php_unit::Opcode` is (#2025):
/// the instrument reads no generated file, so the type's definition is
/// out of its sight, and the inherent impl in `kind.rs` is joined to it
/// by the name the including module re-exports. `Kind::COUNT` has its
/// line and `Kind::name` has none, so the reading names it; the private
/// method is no line.
#[test]
fn a_method_of_a_type_included_from_out_dir_without_a_lock_line_fails() {
    let readings = readings("include");
    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Fail, "{}", heap.message);
    assert!(heap.message.contains("Kind::name"), "{}", heap.message);
    assert!(!heap.message.contains("COUNT"), "{}", heap.message);
    assert!(!heap.message.contains("private"), "{}", heap.message);
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// One order for a lock's lines (#403): `--write` renders them ascending
/// and a check that read the set alone accepted a file it would rewrite,
/// which is how `crates/php_heap/heap.lock` carried `Arr::live_from`
/// before `Arr::len` into every commit whose write was taken for another
/// seam. The `lock-order` fixture has a line for every item and the two
/// lines the wrong way round, so nothing is missing and nothing is stale;
/// what fails is the order, and the reading names both lines. The
/// positive control is the `clean` fixture's own `heap.lock`, held by
/// `the_clean_fixture_passes_every_rule`.
#[test]
fn a_lock_in_an_order_write_would_rewrite_fails_naming_the_line_and_its_place() {
    let readings = readings("lock-order");
    let heap = of(&readings, "heap");
    assert_eq!(heap.status, Status::Fail, "{}", heap.message);
    assert!(
        heap.message
            .contains("crates/php_heap/heap.lock is not in the order")
            && heap.message.contains("alloc stands where Arena is written"),
        "{}",
        heap.message
    );
    assert!(
        !heap.message.contains("have no line") && !heap.message.contains("name no"),
        "every item has its line: {}",
        heap.message
    );
    assert_eq!(failures(&readings).len(), 1);
}

/// `--write` stamps a seam's reading over the lock it wrote, with a
/// snapshot taken after the write (#2163): the reading is fresh for the
/// tree `--write` left, where one stamped with the run's snapshot, taken
/// before, would be stale for its own rewrite.
#[test]
fn a_rewritten_lock_is_stamped_over_what_write_left() {
    let manifest = Manifest::load(workspace()).unwrap();
    let root = Tree::new("api-check-write").unwrap();
    xtask::scratch::repository(&fixture("lock-order"), &root, &["."]).unwrap();
    let home = manifest.scope_path("heap").unwrap();
    let before = Snapshot::disk(&root).unwrap().key(&home).unwrap();
    let readings = api_check::hold(&manifest, &root, true).unwrap();
    let after = Snapshot::disk(&root).unwrap().key(&home).unwrap();
    assert_ne!(before, after, "--write rewrote the lock out of order");
    assert_eq!(of(&readings, "heap").key(), after);
}

/// The api seam's ceilings as the tracked manifest carries them.
fn api_ceiling(manifest: &Manifest) -> LockCeiling {
    let seam = manifest
        .seams
        .iter()
        .find(|s| s.name == "api")
        .expect("the manifest has the api seam");
    manifest
        .lock_ceiling(seam)
        .expect("the api seam has a ceiling")
}

/// A scratch workspace whose `php_ext` has `beside` public items outside
/// `__private` and `private` under it, every one with its lock line, laid
/// down beside the `ceiling` fixture's `Cargo.toml`; the api reading.
fn api_lock_of(manifest: &Manifest, beside: u64, private: u64) -> Reading {
    // A reading hashes its scope's tree through git, so the scratch tree
    // is a repository, the way every scratch workspace here is.
    let scratch = Tree::new("api-check-ceiling").unwrap();
    scratch_workspace(workspace(), &scratch).unwrap();
    let crate_dir = scratch.join("crates/php_ext");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::copy(
        fixture("ceiling").join("crates/php_ext/Cargo.toml"),
        crate_dir.join("Cargo.toml"),
    )
    .unwrap();
    let mut lib = format!("//! {beside} public items, and {private} under `__private`.\n");
    let mut lines = Vec::new();
    for i in 0..beside {
        writeln!(lib, "pub const ITEM_{i:04}: u8 = 0;").unwrap();
        lines.push(format!("ITEM_{i:04}"));
    }
    lib.push_str("pub mod __private {\n");
    for i in 0..private {
        writeln!(lib, "    pub const HELPER_{i:04}: u8 = 0;").unwrap();
        lines.push(format!("__private::HELPER_{i:04}"));
    }
    lib.push_str("}\n");
    lines.sort();
    let lock = format!(
        "# every item with a line; the ceiling is what fails.\n{}\n",
        lines.join("\n")
    );
    fs::write(crate_dir.join("src/lib.rs"), lib).unwrap();
    fs::write(crate_dir.join("api.lock"), lock).unwrap();

    let readings = api_check::hold(manifest, &scratch, false).unwrap();
    of(&readings, "api").clone()
}

/// The `ceiling` fixture is one item past `[budget] api_items` whatever the
/// tracked manifest carries (#338): the fixture directory holds the
/// crate's `Cargo.toml`, and the items and their lock lines are laid down
/// here in a scratch workspace from the ceiling the manifest reads, so a
/// person moving the ceiling moves the fixture with it. The `__private`
/// lines stand at their own cap and are not what fails.
#[test]
fn api_lock_over_the_ceiling_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    let ceiling = api_ceiling(&manifest);
    let private = ceiling
        .private
        .expect("the manifest carries api_private_items");
    let items = ceiling.items + 1;

    let api = api_lock_of(&manifest, items, private);
    assert_eq!(api.status, Status::Fail, "{}", api.message);
    assert_eq!(api.numbers["items"], i64::try_from(items).unwrap().into());
    assert_eq!(
        api.numbers["private"],
        i64::try_from(private).unwrap().into()
    );
    assert!(api.message.contains(&items.to_string()), "{}", api.message);
    assert!(
        api.message
            .contains(&format!("{} (api_items)", ceiling.items)),
        "{}",
        api.message
    );
    assert!(
        !api.message.contains("api_private_items"),
        "{}",
        api.message
    );
}

/// The same fixture one `__private` line past `[budget]
/// api_private_items`, with the rest at `api_items` exactly: the lock
/// fails by the private cap's name alone, so neither side is counted
/// against the other's ceiling.
#[test]
fn api_lock_private_lines_over_their_own_cap_fail() {
    let manifest = Manifest::load(workspace()).unwrap();
    let ceiling = api_ceiling(&manifest);
    let private = ceiling
        .private
        .expect("the manifest carries api_private_items")
        + 1;

    let api = api_lock_of(&manifest, ceiling.items, private);
    assert_eq!(api.status, Status::Fail, "{}", api.message);
    assert!(
        api.message.contains(&format!(
            "{private} __private items; the ceiling is {} (api_private_items)",
            private - 1
        )),
        "{}",
        api.message
    );
    assert!(!api.message.contains("(api_items)"), "{}", api.message);

    let at_both = api_lock_of(&manifest, ceiling.items, private - 1);
    assert_eq!(at_both.status, Status::Pass, "{}", at_both.message);
}

#[test]
fn a_crate_depending_outside_its_edges_fails_with_the_adr() {
    let readings = readings("edge");
    let json = of(&readings, "ext/json");
    assert_eq!(json.status, Status::Fail);
    assert!(json.message.contains("php_engine"), "{}", json.message);
    assert!(
        json.message
            .contains("ADR-0004: Extensions are one crate each under `ext/`"),
        "the ADR's text is printed: {}",
        json.message
    );
    assert_eq!(json.adr, "0004");
}

/// A crate no `[[edge]]` row names may depend on no workspace crate
/// (design §4.2): the harness, which the tracked manifest gives no row
/// since #2213, linking `php_bundle` back in fails naming the missing
/// row, and nothing else in the fixture does.
#[test]
fn a_crate_with_no_edge_row_depending_on_a_workspace_crate_fails() {
    let readings = readings("harness-edge");
    let harness = of(&readings, "xtask");
    assert_eq!(harness.status, Status::Fail, "{}", harness.message);
    assert!(
        harness
            .message
            .contains("xtask depends on php_bundle; no [[edge]] row gives xtask a dependency"),
        "{}",
        harness.message
    );
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The workspace's lints reach a member only through its own
/// `[lints] workspace = true` (#1998, #2111): ext/json, without the line,
/// fails naming itself, and ext/ctype, with it, passes. The tracked
/// workspace, every member carrying the line, is held by
/// `the_workspace_today_has_no_failing_reading`.
#[test]
fn a_crate_that_does_not_inherit_the_workspace_lints_fails_naming_it() {
    let readings = readings("lints");
    let json = of(&readings, "ext/json");
    assert_eq!(json.status, Status::Fail, "{}", json.message);
    assert!(
        json.message
            .contains("ext/json's Cargo.toml lacks `[lints] workspace = true`"),
        "{}",
        json.message
    );
    assert_eq!(of(&readings, "ext/ctype").status, Status::Pass);
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// A host reaches the engine through its facade and never past it
/// (ADR-0008): `sapi/cli` naming `php_engine` fails with the ADR, and
/// `sapi/cgi` on the facade, the codecs and the bundle is within its
/// edges.
#[test]
fn a_host_depending_on_the_engine_past_its_facade_fails_with_the_adr() {
    let readings = readings("host-edge");
    let cli = of(&readings, "sapi/cli");
    assert_eq!(cli.status, Status::Fail, "{}", cli.message);
    assert!(cli.message.contains("php_engine"), "{}", cli.message);
    assert!(
        cli.message
            .contains("ADR-0008: Hosts are one crate each under `sapi/`"),
        "the ADR's text is printed: {}",
        cli.message
    );
    assert_eq!(cli.adr, "0008");
    let cgi = of(&readings, "sapi/cgi");
    assert_eq!(cgi.status, Status::Pass, "{}", cgi.message);
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

#[test]
fn a_use_path_across_a_denied_module_edge_fails() {
    let readings = readings("deny-edge");
    let engine = of(&readings, "php_engine");
    assert_eq!(engine.status, Status::Fail);
    assert!(engine.message.contains("executor"), "{}", engine.message);
    assert!(engine.message.contains("serve"), "{}", engine.message);
    assert!(
        engine.message.contains("src/executor.rs"),
        "the file is named: {}",
        engine.message
    );
}

/// The tracked manifest with `php_bundle`'s edge read as `order`, so a
/// test holds the order it names whatever the tracked row says.
fn under(order: &str) -> Manifest {
    let mut manifest = Manifest::load(workspace()).unwrap();
    let edge = manifest
        .edges
        .iter_mut()
        .find(|e| e.from == "php_bundle")
        .unwrap();
    edge.order = Some(order.to_owned());
    manifest
}

/// The tracked manifest with `php_bundle`'s edge read as
/// `order = "registration"`, the value #1778 ruled.
fn under_registration() -> Manifest {
    under("registration")
}

/// `order = "registration"`: each extension's feature switches on the
/// features of every extension it declares `Required` (#1915). standard's
/// feature omits random, which ext/standard declares Required in rows a
/// function returns, and fails; session's switches on standard, declared
/// Required through a function of its own, and passes; random, which ext/session links
/// and declares nowhere, and apc, which it declares Optional, require
/// nothing; and standard listed before random is no failure.
#[test]
fn a_bundle_feature_that_does_not_switch_on_what_its_extension_requires_fails() {
    let readings = api_check::hold(&under_registration(), &fixture("registration"), false).unwrap();
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
    assert!(
        bundle.message.contains(
            "the feature standard registers ext/standard but does not switch on random, the feature that registers ext/random, which ext/standard requires (design §4.10)"
        ),
        "{}",
        bundle.message
    );
    assert!(
        !bundle.message.contains("ext/session"),
        "session's feature switches on standard, and a link or an Optional row requires nothing: {}",
        bundle.message
    );
    assert!(
        !bundle.message.contains("topological"),
        "{}",
        bundle.message
    );
    assert_eq!(bundle.numbers["extensions"], 3.into());
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// Under `order = "registration"` the order written is not held to the
/// crate graph: the `bundle` fixture, which lists ext/standard before
/// ext/random, which it depends on, behind no feature, passes.
#[test]
fn the_registration_order_does_not_hold_the_order_written() {
    let readings = api_check::hold(&under_registration(), &fixture("bundle"), false).unwrap();
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Pass, "{}", bundle.message);
}

/// The `registration` fixture with the bundle's `src/lib.rs` replaced by
/// `enabled`, in a scratch workspace, held under `order = "registration"`:
/// the variants of the fixture's one rule that each need a bundle of
/// their own.
fn bundle_reading(scratch: &str, enabled: &str) -> Reading {
    bundle_reading_with(scratch, enabled, &[])
}

/// [`bundle_reading`] with the files `replaced` written over the
/// fixture's, each a path in the workspace and its new text.
fn bundle_reading_with(scratch: &str, enabled: &str, replaced: &[(&str, &str)]) -> Reading {
    let root = Tree::new(scratch).unwrap();
    scratch_workspace(workspace(), &root).unwrap();
    let from = fixture("registration");
    for file in [
        "crates/php_bundle/Cargo.toml",
        "ext/standard/Cargo.toml",
        "ext/standard/src/lib.rs",
        "ext/random/Cargo.toml",
        "ext/random/src/lib.rs",
        "ext/session/Cargo.toml",
        "ext/session/src/lib.rs",
    ] {
        fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        fs::copy(from.join(file), root.join(file)).unwrap();
    }
    fs::create_dir_all(root.join("crates/php_bundle/src")).unwrap();
    fs::write(root.join("crates/php_bundle/src/lib.rs"), enabled).unwrap();
    for (file, text) in replaced {
        fs::write(root.join(file), text).unwrap();
    }
    let readings = api_check::hold(&under_registration(), &root, false).unwrap();
    of(&readings, "php_bundle").clone()
}

/// An extension the bundle does not register at all, required by one it
/// does, fails.
#[test]
fn a_required_extension_the_bundle_does_not_register_fails() {
    let bundle = bundle_reading(
        "api-check-unregistered",
        "pub fn enabled() -> &'static [fn()] {\n    &[\n        #[cfg(feature = \"standard\")]\n        php_ext_standard::extension,\n    ]\n}\n",
    );
    assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
    assert!(
        bundle
            .message
            .contains("the bundle registers ext/standard but not ext/random, which it requires"),
        "{}",
        bundle.message
    );
}

/// A `cfg` the instrument does not read fails closed: random behind
/// `cfg(all(...))` is named, never taken for registered in every build.
#[test]
fn a_registration_behind_a_cfg_api_check_does_not_read_fails() {
    let bundle = bundle_reading(
        "api-check-unread-cfg",
        "pub fn enabled() -> &'static [fn()] {\n    &[\n        #[cfg(all(feature = \"random\", unix))]\n        php_ext_random::extension,\n    ]\n}\n",
    );
    assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
    assert!(
        bundle
            .message
            .contains("the bundle registers ext/random behind cfg(all")
            && bundle.message.contains("which api-check does not read"),
        "{}",
        bundle.message
    );
}

/// A declaration the instrument does not read fails closed: rows handed
/// to `.dependencies(...)` from a constant are named, never taken for
/// "requires nothing".
#[test]
fn a_declaration_api_check_does_not_read_fails() {
    let bundle = bundle_reading_with(
        "api-check-unread-declaration",
        "pub fn enabled() -> &'static [fn()] {\n    &[\n        #[cfg(feature = \"random\")]\n        php_ext_random::extension,\n        #[cfg(feature = \"standard\")]\n        php_ext_standard::extension,\n    ]\n}\n",
        &[(
            "ext/standard/src/lib.rs",
            "pub fn extension() -> Extension {\n    ExtensionBuilder::default().dependencies(DEPENDENCIES.to_vec()).build(\"standard\")\n}\n",
        )],
    );
    assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
    assert!(
        bundle.message.contains(
            "ext/standard declares a dependency in src/lib.rs that api-check does not read"
        ),
        "{}",
        bundle.message
    );
}

/// A crate's link to an extension it declares nowhere requires nothing,
/// as `ext/sqlite3`'s to `ext/pdo_sqlite` (#1915): session registered behind
/// a feature that switches standard on and random off passes.
#[test]
fn a_link_with_no_declaration_requires_nothing() {
    let bundle = bundle_reading(
        "api-check-link-alone",
        "pub fn enabled() -> &'static [fn()] {\n    &[\n        #[cfg(feature = \"standard\")]\n        php_ext_standard::extension,\n        #[cfg(feature = \"standard\")]\n        php_ext_random::extension,\n        #[cfg(feature = \"session\")]\n        php_ext_session::extension,\n    ]\n}\n",
    );
    assert_eq!(bundle.status, Status::Pass, "{}", bundle.message);
}

/// Every registration is read, a second one of the same extension
/// included: random registered behind standard's own feature as well as
/// its own is switched on by standard, and session, registered twice,
/// is held at both places.
#[test]
fn each_registration_of_one_extension_is_read() {
    let bundle = bundle_reading(
        "api-check-twice",
        "pub fn enabled() -> &'static [fn()] {\n    &[\n        #[cfg(feature = \"random\")]\n        php_ext_random::extension,\n        #[cfg(feature = \"standard\")]\n        php_ext_random::extension,\n        #[cfg(feature = \"standard\")]\n        php_ext_standard::extension,\n        #[cfg(feature = \"session\")]\n        php_ext_session::extension,\n        php_ext_session::extension,\n    ]\n}\n",
    );
    assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
    assert!(
        !bundle.message.contains("the feature standard"),
        "random's second registration is behind standard: {}",
        bundle.message
    );
    assert!(
        bundle.message.contains(
            "the bundle registers ext/session behind no feature but ext/standard behind standard, which ext/session requires (design §4.10)"
        ),
        "{}",
        bundle.message
    );
    assert_eq!(bundle.numbers["extensions"], 3.into());
}

/// An order the instrument does not serve is refused, never read as one
/// it does: `"topological"`, retired by #1882 and its code gone since
/// #2255, among them.
#[test]
fn an_order_api_check_does_not_serve_fails() {
    for order in ["alphabetical", "topological"] {
        let readings = api_check::hold(&under(order), &fixture("clean"), false).unwrap();
        let bundle = of(&readings, "php_bundle");
        assert_eq!(bundle.status, Status::Fail, "{}", bundle.message);
        assert!(
            bundle.message.contains(&format!(
                "order = \"{order}\" is not an order api-check knows; it serves \"registration\" alone"
            )),
            "{}",
            bundle.message
        );
    }
}

/// The tracked workspace passes under `order = "registration"` as under
/// the manifest's own value, so a person's flip of the row fails nothing
/// (#1882, #1778).
#[test]
fn the_workspace_today_passes_under_the_registration_order() {
    let readings = api_check::hold(&under_registration(), workspace(), false).unwrap();
    let bundle = of(&readings, "php_bundle");
    assert_eq!(bundle.status, Status::Pass, "{}", bundle.message);
}

/// The tracked workspace: every seam whose lock has no home present yet,
/// or that is generated from a file the harness renders no lock from, is
/// a skip, `tier.lock`, diffed per section, among those measured (#1369);
/// and the harness itself is within its edges.
#[test]
fn the_workspace_today_has_no_failing_reading() {
    let manifest = Manifest::load(workspace()).unwrap();
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
    assert_eq!(of(&readings, "xtask").status, Status::Pass);
    assert_eq!(of(&readings, "tier").status, Status::Pass);
    let unmeasured = manifest
        .seams
        .iter()
        .filter(|seam| {
            (seam.generated && seam.producer != "ops.toml")
                || manifest
                    .lock_home(&seam.lock)
                    .and_then(|home| manifest.crate_named(&home))
                    .is_none_or(|home| !workspace().join(home.path()).join("Cargo.toml").is_file())
        })
        .count();
    assert_eq!(
        readings.iter().filter(|r| r.status == Status::Skip).count(),
        unmeasured,
        "one skip per seam without a home present or measured: {readings:#?}"
    );
}

/// A hand-edited line of `ops.lock`: the one lock rendered from a file
/// rather than from a crate's sources, held by the same instrument as
/// the other six (#95). The fixture's `ADD` line has lost its `throw`
/// effect, so the rendered line has no line in the lock and the line
/// names no opcode.
#[test]
fn a_hand_edited_line_of_ops_lock_fails() {
    let readings = readings("ops-lock");
    let ops = of(&readings, "ops");
    assert_eq!(ops.status, Status::Fail, "{}", ops.message);
    assert_eq!(ops.numbers["items"], 3.into());
    assert!(
        ops.message.contains("crates/php_unit/ops.lock"),
        "the lock is named: {}",
        ops.message
    );
    assert!(
        ops.message.contains("alloc,throw"),
        "the rendered line is named: {}",
        ops.message
    );
    assert!(
        ops.message.contains("name no opcode"),
        "the hand edit is named as stale: {}",
        ops.message
    );
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The `ops` seam's lock lives with `php_unit`, which produces no part of
/// it: the `[lock_home]` table names the home, since the seam's producer
/// is a file and no `[[crate]]` row carries the lock.
#[test]
fn the_ops_lock_is_at_home_with_php_unit() {
    let manifest = Manifest::load(workspace()).unwrap();
    assert_eq!(
        manifest.lock_home("ops.lock").as_deref(),
        Some("php_unit"),
        "the manifest names the lock's home"
    );
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let ops = of(&readings, "ops");
    assert_eq!(ops.status, Status::Pass, "{}", ops.message);
    assert_eq!(ops.numbers["items"], 209.into());
    assert!(
        ops.message.contains("crates/php_unit/ops.lock"),
        "{}",
        ops.message
    );
}

/// The rows a person adds for the harness's own lock (#55, harness §11:
/// the agent proposes, the person commits), on a manifest that does not
/// carry them yet: the `harness` seam over `xtask.lock`, the lock on the
/// `xtask` crate row, and the `xtask.lock-line` rule that notes a new
/// public item at the call. Once the tracked manifest carries them
/// this adds nothing, and the two tests below hold the tracked rows.
fn with_the_harness_rows(mut manifest: Manifest) -> Manifest {
    if !manifest.seams.iter().any(|s| s.lock == "xtask.lock") {
        let mut seam = Seam::named("harness", "xtask");
        "xtask.lock".clone_into(&mut seam.lock);
        seam.consumers = vec!["xtask".to_owned()];
        manifest.seams.push(seam);
    }
    let row = manifest
        .crates
        .iter_mut()
        .find(|c| c.name == "xtask")
        .expect("the manifest has the xtask row");
    if !row.lock.iter().any(|l| l == "xtask.lock") {
        row.lock = OneOrMany::One("xtask.lock".to_owned());
    }
    if manifest.rule("xtask.lock-line").is_none() {
        let mut rule = Rule::named("xtask.lock-line", "xtask/src/**");
        rule.predicate.deny = None;
        rule.predicate.on = Some("pub".to_owned());
        "A new pub item in xtask needs its xtask.lock line in the same commit."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

/// The negative control of the harness's lock: a public item with no line
/// in `xtask/xtask.lock` fails the `harness` seam, the reading names the
/// lock and the item and carries the `on = "pub"` rule over `xtask/src`,
/// and a `pub(crate)` item is no public item.
#[test]
fn a_public_item_of_the_harness_without_its_lock_line_fails_naming_xtask_lock() {
    let manifest = with_the_harness_rows(Manifest::load(workspace()).unwrap());
    let readings = api_check::hold(&manifest, &fixture("harness-lock"), false).unwrap();
    let harness = of(&readings, "harness");
    assert_eq!(harness.status, Status::Fail, "{}", harness.message);
    assert!(
        harness.message.contains("xtask/xtask.lock"),
        "{}",
        harness.message
    );
    assert!(harness.message.contains("measure"), "{}", harness.message);
    assert!(
        !harness.message.contains("inside"),
        "pub(crate) is not public: {}",
        harness.message
    );
    assert_eq!(harness.rule, "xtask.lock-line");
    assert_eq!(harness.numbers["items"], 2.into());
    assert_eq!(failures(&readings).len(), 1, "{readings:#?}");
}

/// The committed `xtask/xtask.lock` is the harness's public items as they
/// stand: held here under the rows before a person commits them, so a new
/// public item without its line fails this test the way it will fail
/// `api-check` once the row is the manifest's, and `xtask api-check
/// --write` is what regenerates it then.
#[test]
fn the_harness_lock_is_current_over_the_tracked_workspace() {
    let manifest = with_the_harness_rows(Manifest::load(workspace()).unwrap());
    let readings = api_check::hold(&manifest, workspace(), false).unwrap();
    let harness = of(&readings, "harness");
    assert_eq!(
        harness.status,
        Status::Pass,
        "{}; a new public item of xtask needs its line in xtask/xtask.lock in the same commit",
        harness.message
    );
    assert!(
        harness.message.contains("xtask/xtask.lock"),
        "{}",
        harness.message
    );
    assert_eq!(harness.rule, "xtask.lock-line");
    assert_eq!(
        harness.key(),
        of(&readings, "xtask").key(),
        "the lock's reading is stamped over its home, the harness"
    );
    let failed: Vec<String> = failures(&readings)
        .iter()
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect();
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// A sectioned lock is measured per section (#1369, #1350's ruling):
/// `tier.lock`'s face holds the `tier::*` items `php_sapi` imports, and
/// `sapi.lock` does not. The `tier-face` fixture's `php_sapi` imports
/// `tier::Site`, which neither lock lists, so the `tier` reading fails
/// naming it under `[face]`, and `sapi` names it too, the one import no
/// lock holds; `tier::Tier`, which the face lists, is not `sapi.lock`'s.
#[test]
fn a_tier_face_item_the_lock_does_not_list_fails_the_tier_reading() {
    let readings = readings("tier-face");
    let tier = of(&readings, "tier");
    assert_eq!(tier.status, Status::Fail, "{}", tier.message);
    assert!(
        tier.message.contains(
            "1 imports from php_engine have no line in crates/php_engine/tier.lock [face]: tier::Site"
        ),
        "{}",
        tier.message
    );
    let sapi = of(&readings, "sapi");
    assert_eq!(sapi.status, Status::Fail, "{}", sapi.message);
    assert!(
        sapi.message.contains("tier::Site") && !sapi.message.contains("tier::Tier"),
        "{}",
        sapi.message
    );
}

/// The positive control and the section a consumer does not read: with
/// `tier::Site` under `[face]` both readings pass; under `[kernels]`, when
/// the manifest gives that section to `php_jit` alone and a `kernels!` list
/// of `php_jit`'s names it (#2957), `php_sapi`'s import of it fails the
/// `tier` reading.
#[test]
fn each_section_is_held_against_the_consumers_that_read_it() {
    let root = Tree::new("api-check-tier-face").unwrap();
    scratch_workspace(workspace(), &root).unwrap();
    let from = fixture("tier-face");
    for file in [
        "crates/php_engine/Cargo.toml",
        "crates/php_engine/src/lib.rs",
        "crates/php_engine/src/tier.rs",
        "crates/php_engine/sapi.lock",
        "crates/php_sapi/Cargo.toml",
        "crates/php_sapi/src/lib.rs",
    ] {
        fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        fs::copy(from.join(file), root.join(file)).unwrap();
    }
    let lock = root.join("crates/php_engine/tier.lock");
    let mut manifest = Manifest::load(workspace()).unwrap();

    fs::write(&lock, "[face]\ntier::Site\ntier::Tier\n[kernels]\n").unwrap();
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    for scope in ["tier", "sapi"] {
        let reading = of(&readings, scope);
        assert_eq!(reading.status, Status::Pass, "{scope}: {}", reading.message);
    }

    let tier = manifest
        .seams
        .iter_mut()
        .find(|s| s.name == "tier")
        .unwrap();
    tier.sections = vec![
        Section::Every("face".to_owned()),
        Section::Only {
            name: "kernels".to_owned(),
            consumers: vec!["php_jit".to_owned()],
        },
    ];
    let jit = root.join("crates/php_jit");
    fs::create_dir_all(jit.join("src")).unwrap();
    let manifest_of_jit = fs::read_to_string(from.join("crates/php_sapi/Cargo.toml")).unwrap();
    fs::write(
        jit.join("Cargo.toml"),
        manifest_of_jit.replace("php_sapi", "php_jit"),
    )
    .unwrap();
    let list = "kernels! { Request => mod wrapped { Site => site() -> u64 = tier::Site; } }\n";
    fs::write(jit.join("src/lib.rs"), list).unwrap();
    fs::write(&lock, "[face]\ntier::Tier\n[kernels]\ntier::Site\n").unwrap();
    let readings = api_check::hold(&manifest, &root, false).unwrap();
    let tier = of(&readings, "tier");
    assert_eq!(tier.status, Status::Fail, "{}", tier.message);
    assert!(
        tier.message.contains(
            "php_sapi imports tier::Site, which crates/php_engine/tier.lock lists under [kernels], read by php_jit alone"
        ),
        "{}",
        tier.message
    );
}

/// `tier.lock`'s kernels section is generated from `php_jit`'s `kernels!`
/// lists (jit-design.md §5.3, #2957), never kept: the `tier-kernels`
/// fixture's list names `ops::sub_slow`, which its `[kernels]` lists, so
/// that import holds; `ops::add_slow`, which `php_jit` imports and
/// `[kernels]` lists by hand, is named by no list, so the reading fails
/// naming it twice, as an import no `[face]` line holds and as a
/// `[kernels]` line no list names.
#[test]
fn a_kernel_no_kernels_list_names_fails_the_tier_reading() {
    let readings = readings("tier-kernels");
    let tier = of(&readings, "tier");
    assert_eq!(tier.status, Status::Fail, "{}", tier.message);
    for said in [
        "1 imports from php_engine have no line in crates/php_engine/tier.lock [face]: ops::add_slow",
        "1 lines of crates/php_engine/tier.lock [kernels] name no kernel a kernels! list names: ops::add_slow",
    ] {
        assert!(tier.message.contains(said), "{}", tier.message);
    }
    assert!(!tier.message.contains("sub_slow"), "{}", tier.message);
}

/// The tracked manifest with a seam that is one module of `php_engine`,
/// `layer`, read by `php_streams` alone (#1959).
fn with_the_layer_seam() -> Manifest {
    let mut manifest = Manifest::load(workspace()).unwrap();
    manifest.seams.retain(|s| s.name != "layer");
    let mut layer = Seam::named("layer", "php_engine");
    layer.consumers = vec!["php_streams".to_owned()];
    layer.module = Some("layer".to_owned());
    manifest.seams.push(layer);
    manifest
}

/// A seam that is one module of its producer is an item lock of that
/// module alone, living with the producer (#1959). The `layer-face`
/// fixture's `php_engine::layer` defines a sealed trait with one method and
/// a free function, its lock lists the trait and the method and not the
/// function, and `php_ext`, which is not the seam's consumer, re-exports the
/// function: the reading fails naming both, and leaves `api::Request` out.
#[test]
fn a_module_seam_holds_the_module_alone_and_refuses_a_stranger_using_it() {
    let manifest = with_the_layer_seam();
    assert_eq!(
        manifest.lock_home("layer.lock").as_deref(),
        Some("php_engine"),
        "a module seam's lock lives with its producer"
    );
    let readings = api_check::hold(&manifest, &fixture("layer-face"), false).unwrap();
    let layer = of(&readings, "layer");
    assert_eq!(layer.status, Status::Fail, "{}", layer.message);
    assert_eq!(layer.numbers["items"], 3.into(), "{}", layer.message);
    assert!(
        layer.message.contains(
            "1 public items of php_engine have no line in crates/php_engine/layer.lock: layer::poll_fd"
        ),
        "{}",
        layer.message
    );
    assert!(
        layer.message.contains(
            "crates/php_ext/src/lib.rs uses php_engine::layer::poll_fd; php_engine::layer is php_streams's alone"
        ),
        "{}",
        layer.message
    );
    assert!(!layer.message.contains("Request"), "{}", layer.message);
    assert!(
        !layer.message.contains("crates/php_streams"),
        "the consumer's use is the seam's: {}",
        layer.message
    );
}

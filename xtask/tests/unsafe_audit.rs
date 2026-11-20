//! `xtask unsafe-audit` over the tracked workspace (design §4.1).

use xtask::manifest::{Crate, HomeGroup, Manifest, Rule};
use xtask::reading::{Number, Outcome, Status};
use xtask::unsafe_audit;

mod common {
    pub mod repo;
}
use common::repo::Repo;

/// A count of a reading, which `unsafe-audit` writes as an integer.
fn count(number: &Number) -> i64 {
    match number {
        Number::Int(n) => *n,
        Number::Float(_) => panic!("{number:?} is no count"),
    }
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red.
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
#[cfg(tracked)]
mod tracked {
    use super::*;

    fn workspace() -> &'static std::path::Path {
        xtask::package_dir().parent().unwrap()
    }

    /// The group of each home the tracked `unsafe.homes` names, as proposed
    /// for a manifest that does not carry `group` yet: the engine's own
    /// homes, and the bindings and `timelib_sys`, which call a C library.
    const PROPOSED: [(&str, HomeGroup); 8] = [
        ("php_heap", HomeGroup::Engine),
        ("php_process", HomeGroup::Engine),
        ("ext/*/src/ffi/", HomeGroup::Foreign),
        ("sapi/*/src/ffi/", HomeGroup::Foreign),
        ("timelib_sys", HomeGroup::Foreign),
        ("php_jit::table::code", HomeGroup::Engine),
        ("php_jit::enter", HomeGroup::Engine),
        ("xtask::bench::spent::read", HomeGroup::Engine),
    ];

    /// The tracked manifest, with [`PROPOSED`] as its `group` where it
    /// carries none; once it carries one, that one stands.
    fn grouped() -> Manifest {
        let mut manifest = Manifest::load(workspace()).unwrap();
        let rule = (manifest.rules.iter_mut())
            .find(|r| r.id == "unsafe.homes")
            .unwrap();
        let group = PROPOSED.map(|(home, g)| (home.to_owned(), g));
        rule.predicate.group.get_or_insert_with(|| group.into());
        manifest
    }

    #[test]
    fn the_tracked_workspace_has_no_unsafe_outside_the_homes() {
        let manifest = grouped();
        let readings = unsafe_audit::run(workspace(), &manifest).unwrap();
        assert_eq!(
            readings.len(),
            manifest.crates.len() + 1,
            "one reading per crate and the workspace's"
        );
        let shares = readings.iter().find(|r| r.scope == "workspace").unwrap();
        assert_eq!(shares.status, Status::Pass, "{shares:#?}");
        let n = |key: &str| count(&shares.numbers[key]);
        assert_eq!(
            n("safe_lines") + n("engine_lines") + n("foreign_lines"),
            n("total_lines"),
            "{shares:#?}"
        );
        for key in ["safe_lines", "engine_lines", "foreign_lines"] {
            assert!(n(key) > 0, "{key}: {shares:#?}");
        }
        assert!(n("engine_blocks") > 0 && n("foreign_blocks") > 0);
        for reading in readings.iter().filter(|r| r.scope != "workspace") {
            assert_eq!(reading.rule, "unsafe.homes");
            assert_eq!(reading.adr, "0006", "the ADR that decided the homes");
            assert_ne!(reading.status, Status::Fail, "{reading:#?}");
        }
        let xtask = readings.iter().find(|r| r.scope == "xtask").unwrap();
        assert_eq!(xtask.status, Status::Pass);
        // The bench reader's eight, in its home `xtask::bench::spent::read`
        // (ruled 2026-10-20): four reading a server's totals, two waiting on
        // a process arm unreaped to read its own, and two waiting on one on
        // Linux to read its peak resident set; on another platform a part
        // is not compiled, but its source is read all the same.
        assert_eq!(xtask.numbers["sites"], 8.into());
        assert_eq!(xtask.numbers["outside"], 0.into());
        assert!(!xtask.key().is_empty(), "stamped");
    }

    /// A host's `unsafe` lives under `sapi/*/src/ffi/` and nowhere else
    /// (ADR-0008), under the home proposed for a manifest that does not
    /// carry it yet, which this adds and adds nothing once it does:
    /// `sapi/fpm` with its block in `src/ffi/` and `#![deny(unsafe_code)]`
    /// at its root passes, and `sapi/phpdbg` with one in `src/lib.rs` fails
    /// naming the line.
    #[test]
    fn a_host_s_unsafe_is_at_home_under_its_ffi_directory_alone() {
        let tree = xtask::scratch::Tree::new("unsafe-audit-hosts").unwrap();
        let write = |rel: &str, text: &str| {
            let path = tree.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "sapi/fpm/Cargo.toml",
            "[package]\nname = \"php_sapi_fpm\"\n",
        );
        write(
            "sapi/fpm/src/lib.rs",
            "#![deny(unsafe_code)]\n#[allow(unsafe_code)]\nmod ffi;\n",
        );
        write(
            "sapi/fpm/src/ffi/mod.rs",
            "pub fn pid() -> i32 {\n    // SAFETY: the fixture's.\n    unsafe { 0 }\n}\n",
        );
        write(
            "sapi/phpdbg/Cargo.toml",
            "[package]\nname = \"php_sapi_phpdbg\"\n",
        );
        write(
            "sapi/phpdbg/src/lib.rs",
            "#![forbid(unsafe_code)]\npub fn tty() -> i32 {\n    unsafe { 0 }\n}\n",
        );
        Repo::init(&tree).commit("hosts");
        let mut manifest = Manifest::load(workspace()).unwrap();
        manifest.crates = vec![
            xtask::manifest::Crate::named("sapi/fpm"),
            xtask::manifest::Crate::named("sapi/phpdbg"),
        ];
        let homes = manifest
            .rules
            .iter_mut()
            .find(|r| r.id == "unsafe.homes")
            .and_then(|r| r.predicate.allow.as_mut())
            .unwrap();
        if !homes.iter().any(|h| h == "sapi/*/src/ffi/") {
            homes.push("sapi/*/src/ffi/".to_owned());
        }
        let readings = unsafe_audit::run(&tree, &manifest).unwrap();
        let of = |scope: &str| readings.iter().find(|r| r.scope == scope).unwrap();
        assert_eq!(of("sapi/fpm").status, Status::Pass, "{:#?}", of("sapi/fpm"));
        assert_eq!(of("sapi/fpm").numbers["sites"], 1.into());
        let phpdbg = of("sapi/phpdbg");
        assert_eq!(phpdbg.status, Status::Fail, "{phpdbg:#?}");
        assert!(
            phpdbg.message.contains("sapi/phpdbg/src/lib.rs:3"),
            "{}",
            phpdbg.message
        );
    }
}

/// A workspace of three crates: `php_heap`, a home whole, with two blocks
/// and an `unsafe fn` in its source and a test beside it; `ext/pcre`,
/// whose `src/ffi/` is a home with one block; and `php_unit`, which
/// forbids `unsafe`, with a fixture the compiler never builds. Its
/// manifest's `unsafe.homes` assigns each home the group `group` names.
fn shares_tree(name: &str, group: &[(&str, HomeGroup)]) -> (xtask::scratch::Tree, Manifest) {
    let tree = xtask::scratch::Tree::new(name).unwrap();
    let repo = Repo::init(&tree);
    for (krate, package) in [
        ("crates/php_heap", "php_heap"),
        ("crates/php_unit", "php_unit"),
        ("ext/pcre", "php_ext_pcre"),
    ] {
        repo.write(
            &format!("{krate}/Cargo.toml"),
            format!("[package]\nname = \"{package}\"\n"),
        );
    }
    repo.write(
        "crates/php_heap/src/lib.rs",
        "pub fn f() -> i32 {\n    // SAFETY: the fixture's.\n    unsafe { 0 }\n}\npub unsafe fn g() {}\npub fn h() -> i32 {\n    unsafe { 1 }\n}\n",
    );
    repo.write("crates/php_heap/tests/heap.rs", "#[test]\nfn t() {}\n");
    repo.write(
        "ext/pcre/src/lib.rs",
        "#![deny(unsafe_code)]\n#[allow(unsafe_code)]\nmod ffi;\n\npub fn safe() {}\n",
    );
    repo.write(
        "ext/pcre/src/ffi/mod.rs",
        "pub fn len() -> usize {\n    // SAFETY: the fixture's.\n    unsafe { 0 }\n}\n",
    );
    repo.write(
        "crates/php_unit/src/lib.rs",
        "#![forbid(unsafe_code)]\n\npub fn u() {}\n",
    );
    repo.write(
        "crates/php_unit/fixtures/x.rs",
        "fn f() -> i32 { unsafe { 0 } }\n",
    );
    repo.commit("three crates");
    let mut manifest = Manifest::of(["php_heap", "php_unit", "ext/pcre"].map(Crate::named));
    let mut rule = Rule::named("unsafe.homes", "crates/**");
    rule.predicate.deny = None;
    rule.predicate.allow = Some(vec!["php_heap".to_owned(), "ext/*/src/ffi/".to_owned()]);
    if !group.is_empty() {
        let group = group.iter().map(|(home, g)| ((*home).to_owned(), *g));
        rule.predicate.group = Some(group.collect());
    }
    manifest.rules.push(rule);
    (tree, manifest)
}

/// The workspace reading's shares: the lines of Rust outside the homes,
/// in the engine's own and in the ones that call C, tests included and
/// a fixture left out, which sum to the total; and the blocks of each
/// home group, an `unsafe fn` being no block.
#[test]
fn the_shares_are_three_line_counts_summing_to_the_total_and_two_block_counts() {
    let homes = [
        ("php_heap", HomeGroup::Engine),
        ("ext/*/src/ffi/", HomeGroup::Foreign),
    ];
    let (tree, manifest) = shares_tree("unsafe-audit-shares", &homes);
    let readings = unsafe_audit::run(&tree, &manifest).unwrap();
    assert_eq!(readings.len(), 4, "one per crate and the workspace's");
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Pass, "{workspace:#?}");
    let n = |key: &str| count(&workspace.numbers[key]);
    assert_eq!(n("safe_lines"), 6, "pcre's lib.rs and php_unit's");
    assert_eq!(n("engine_lines"), 9, "php_heap's source and its test");
    assert_eq!(n("foreign_lines"), 3, "pcre's ffi");
    assert_eq!(n("total_lines"), 18, "the fixture left out");
    assert_eq!(
        n("safe_lines") + n("engine_lines") + n("foreign_lines"),
        n("total_lines")
    );
    assert_eq!(n("engine_blocks"), 2);
    assert_eq!(n("foreign_blocks"), 1);
    assert!(!workspace.key().is_empty(), "stamped");
    assert_eq!(Outcome::of(&readings), Outcome::Met);
}

/// Each home is in exactly one group: a home the rule's `group` leaves
/// out fails the run naming it, and so does a group whose home the rule
/// does not allow; a rule that groups nothing measures no shares.
#[test]
fn a_home_in_no_group_fails_the_run_naming_it() {
    let partial = [
        ("php_heap", HomeGroup::Engine),
        ("php_jit::enter", HomeGroup::Engine),
    ];
    let (tree, manifest) = shares_tree("unsafe-audit-ungrouped", &partial);
    let readings = unsafe_audit::run(&tree, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Fail, "{workspace:#?}");
    for named in ["ext/*/src/ffi/ is in no group", "php_jit::enter"] {
        assert!(
            workspace.message.contains(named),
            "{named}: {}",
            workspace.message
        );
    }
    assert_ne!(Outcome::of(&readings), Outcome::Met);
    for crate_reading in readings.iter().filter(|r| r.scope != "workspace") {
        assert_eq!(crate_reading.status, Status::Pass, "{crate_reading:#?}");
    }

    let (tree, manifest) = shares_tree("unsafe-audit-no-group", &[]);
    let readings = unsafe_audit::run(&tree, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Skip, "{workspace:#?}");
    assert!(
        workspace.message.contains("no `group`"),
        "{}",
        workspace.message
    );
}

/// A manifest without `unsafe.homes` names no home, so nothing is
/// audited: a skip per crate naming the rule, which the run meets.
/// The crate's `unsafe` outside any home would fail it under the rule.
#[test]
fn a_manifest_without_the_rule_is_a_skip_per_crate_naming_it() {
    let tree = xtask::scratch::Tree::new("unsafe-audit-no-rule").unwrap();
    let repo = Repo::init(&tree);
    repo.write(
        "crates/php_heap/Cargo.toml",
        "[package]\nname = \"php_heap\"\n",
    );
    repo.write(
        "crates/php_heap/src/lib.rs",
        "pub fn f() -> i32 {\n    unsafe { 0 }\n}\n",
    );
    repo.commit("a crate");
    let manifest = Manifest::of([xtask::manifest::Crate::named("php_heap")]);
    let readings = unsafe_audit::run(&tree, &manifest).unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(reading.rule, "unsafe.homes");
    assert!(
        reading.message.contains("no rule unsafe.homes"),
        "{}",
        reading.message
    );
    assert_eq!(Outcome::of(&readings), Outcome::Met);
}

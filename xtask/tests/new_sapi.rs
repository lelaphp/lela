//! `xtask new-sapi` over the fixture of `xtask/fixtures/new-sapi`
//! (ADR-0008): the host it lays down has the topology of the CLI and CGI
//! hosts, passes `budget`, `unsafe-audit`, `api-check` and `clippy`
//! untouched, the last against the fixture's stand-ins for the facade and
//! the bundle, which carry the items the template names (#2524), joins
//! the workspace members, and a second run for the same name refuses, as
//! one for a name without its row does. The fixture is copied to a scratch
//! directory first; nothing is generated in place, and the workspace's own
//! `sapi/` gains no crate. The stand-ins are held to the real crates: each
//! public item of one is a public item or a re-export of the workspace's
//! crate of that name, read from the sources as a lock is (#2544).

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status};
use xtask::scratch::Tree;
use xtask::{api_check, budget, clippy, new_sapi, unsafe_audit};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/new-sapi")
}

/// A scratch copy of the fixture with one commit, since a reading's stamp
/// names `HEAD`.
fn scratch(name: &str) -> Tree {
    let dir = Tree::new(&format!("new-sapi-{name}")).unwrap();
    copy(&fixture(), &dir);
    // The pin is referenced by a path relative to the fixture's place in the tree.
    let seams = std::fs::read_to_string(dir.join("seams.toml")).unwrap();
    let pin = workspace().join("oracle.toml");
    std::fs::write(
        dir.join("seams.toml"),
        seams.replace("../../../oracle.toml", pin.to_str().unwrap()),
    )
    .unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@localhost",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    dir
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn read(root: &Path, rel: &str) -> String {
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

fn failed(readings: &[Reading]) -> Vec<String> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .map(|r| format!("{}: {}", r.scope, r.message))
        .collect()
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of scope {scope}"))
}

#[test]
fn the_host_laid_down_has_the_topology_of_the_cli_and_the_cgi() {
    let root = scratch("topology");
    let manifest = Manifest::load(&root).unwrap();
    let laid = new_sapi::run(&root, &manifest, "demo", None).unwrap();
    assert_eq!(laid.dir, root.join("sapi/demo"));

    // Two dependencies, the facade and the bundle (ADR-0008), and the
    // binary under php's name for the host.
    let cargo: toml::Table = toml::from_str(&read(&root, "sapi/demo/Cargo.toml")).unwrap();
    assert_eq!(cargo["package"]["name"].as_str(), Some("php_sapi_demo"));
    let deps = cargo["dependencies"].as_table().unwrap();
    let mut names: Vec<&str> = deps.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(names, ["php_bundle", "php_sapi"]);
    assert_eq!(
        deps["php_sapi"]["path"].as_str(),
        Some("../../crates/php_sapi")
    );
    let bins = cargo["bin"].as_array().unwrap();
    assert_eq!(bins.len(), 1, "{bins:?}");
    assert_eq!(bins[0]["name"].as_str(), Some("php-demo"));
    assert_eq!(cargo["lints"]["workspace"].as_bool(), Some(true));

    // `main`, the host's name, the facade alone, the lints the manifest
    // denies under sapi/*, and no unsafe.
    let lib = read(&root, "sapi/demo/src/lib.rs");
    assert!(lib.contains("pub fn main() -> ExitCode"), "{lib}");
    assert!(lib.contains("pub const NAME: &str = \"demo\";"), "{lib}");
    assert!(lib.contains("use php_sapi::{"), "{lib}");
    assert!(!lib.contains("php_engine"), "the facade alone: {lib}");
    assert!(lib.contains("#![forbid(unsafe_code)]"), "{lib}");
    let main = read(&root, "sapi/demo/src/main.rs");
    assert!(main.contains("php_sapi_demo::main()"), "{main}");
    // Both crate roots, the library's and the binary's (#2524).
    for lint in manifest
        .rule("lints.no-panic")
        .unwrap()
        .predicate
        .deny_lints
        .as_ref()
        .unwrap()
    {
        for root in [&lib, &main] {
            assert!(
                root.contains(lint),
                "{lint} is denied from the manifest: {root}"
            );
        }
    }
    // No build script: the build's facts are the engine's own stamp, and
    // the bundle's `configure` switches are handed to it.
    assert!(!root.join("sapi/demo/build.rs").exists());
    assert!(lib.contains(".configure(php_bundle::CONFIGURE)"), "{lib}");
    let test = read(&root, "sapi/demo/tests/serve.rs");
    assert!(test.contains("CARGO_BIN_EXE_php-demo"), "{test}");

    // The workspace member, where the root lists them one by one.
    let members: toml::Table = toml::from_str(&read(&root, "Cargo.toml")).unwrap();
    let members = members["workspace"]["members"].as_array().unwrap();
    assert!(
        members.iter().any(|m| m.as_str() == Some("sapi/demo")),
        "{members:?}"
    );

    // The note names the subset the row owns, read from the manifest.
    let note = laid
        .notes
        .iter()
        .find(|n| n.starts_with("phpt subset:"))
        .unwrap_or_else(|| panic!("{:#?}", laid.notes));
    assert!(note.contains("php-src/sapi/cli/tests"), "{note}");
}

#[test]
fn the_binary_is_named_as_the_line_names_it() {
    let root = scratch("binary");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", Some("demodbg")).unwrap();
    let cargo = read(&root, "sapi/demo/Cargo.toml");
    assert!(cargo.contains("name = \"demodbg\""), "{cargo}");
    let test = read(&root, "sapi/demo/tests/serve.rs");
    assert!(test.contains("CARGO_BIN_EXE_demodbg"), "{test}");
}

#[test]
fn the_host_laid_down_passes_the_four_instruments_untouched() {
    let root = scratch("instruments");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", None).unwrap();

    let readings = budget::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "sapi/demo").status, Status::Pass);

    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    assert_eq!(of(&readings, "sapi/demo").status, Status::Pass);

    let readings = api_check::hold(&manifest, &root, false).unwrap();
    assert!(failed(&readings).is_empty(), "{:?}", failed(&readings));
    let demo = of(&readings, "sapi/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);

    // Every target, warnings denied, and the `deny_lints` rules at both
    // crate roots: a `let _ =` over an `io::Result` is `let_underscore_drop`.
    let readings = clippy::run(&root, &manifest, &["sapi/demo".to_owned()]).unwrap();
    let demo = of(&readings, "sapi/demo");
    assert_eq!(demo.status, Status::Pass, "{}", demo.message);
}

/// The public items of the fixture's stand-in for `name` that the
/// workspace's crate of that name lacks, as lock lines; the sources are
/// read, nothing is built.
fn beyond_the_real(fixture: &Path, name: &str) -> Vec<String> {
    let real =
        api_check::public_items(&Manifest::load(workspace()).unwrap(), workspace(), name).unwrap();
    let stand_in =
        api_check::public_items(&Manifest::load(fixture).unwrap(), fixture, name).unwrap();
    assert!(
        !stand_in.is_empty(),
        "the stand-in for {name} declares nothing"
    );
    stand_in.difference(&real).cloned().collect()
}

#[test]
fn the_stand_ins_declare_only_what_the_real_facade_and_bundle_declare() {
    for name in ["php_sapi", "php_bundle"] {
        let beyond = beyond_the_real(&fixture(), name);
        assert!(
            beyond.is_empty(),
            "{name}: not public in the real crate: {beyond:?}"
        );
    }

    // The negative control: an item the real facade lacks, a method of a
    // type it has, and a function of the bundle it lacks.
    let root = scratch("stand-ins");
    for (name, extra) in [
        (
            "php_sapi",
            "pub struct Served;\nimpl Engine {\n    pub fn served(&self) {}\n}\n",
        ),
        ("php_bundle", "pub fn registered() {}\n"),
    ] {
        let lib = root.join(format!("crates/{name}/src/lib.rs"));
        let text = std::fs::read_to_string(&lib).unwrap();
        std::fs::write(&lib, text + extra).unwrap();
    }
    assert_eq!(
        beyond_the_real(&root, "php_sapi"),
        ["Engine::served", "Served"]
    );
    assert_eq!(beyond_the_real(&root, "php_bundle"), ["registered"]);
}

#[test]
fn a_second_run_and_a_host_without_its_row_are_refused_writing_nothing() {
    let root = scratch("refused");
    let manifest = Manifest::load(&root).unwrap();
    new_sapi::run(&root, &manifest, "demo", None).unwrap();
    let again = new_sapi::run(&root, &manifest, "demo", None).unwrap_err();
    assert!(format!("{again:#}").contains("exists"), "{again:#}");

    let rowless = new_sapi::run(&root, &manifest, "fpm", None).unwrap_err();
    assert!(
        format!("{rowless:#}").contains("no [[crate]] row for sapi/fpm"),
        "{rowless:#}"
    );
    assert!(!root.join("sapi/fpm").exists());

    let named = new_sapi::run(&root, &manifest, "Demo", None).unwrap_err();
    assert!(format!("{named:#}").contains("lowercase"), "{named:#}");
}

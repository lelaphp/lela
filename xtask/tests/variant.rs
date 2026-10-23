//! Oracle variants (#1782, #1890): `oracle.toml`'s `[variant.<name>]`, the
//! oracle built with flags of its own and pinned per platform, and a
//! `[[crate]]` row's `oracle` key, which runs that crate's `crosscheck`,
//! `witness` and `phpt` under the variant's binary for the platform they
//! run on, verified by its md5, and every other crate under the main
//! oracle.
//!
//! A variant here is a shell file of the test's own pinned by its md5, so
//! every test runs on any machine of a pinned platform, the oracle's binary
//! there or not; the main oracle is pointed nowhere wherever a test holds a
//! lane to the variant, so a reading that ran is one the variant ran. The
//! two negative controls are the fixtures under `xtask/fixtures/oracle/`: a
//! row naming a variant the pin lacks, and a variant whose binary's md5 is
//! not its pin's.

use std::fs;
use std::path::{Path, PathBuf};

use xtask::crosscheck::{self, Column};
use xtask::divergence::Registry;
use xtask::manifest::{Crate, Manifest};
use xtask::oracle::{Pin, Platform};
use xtask::phpt;
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::witness;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::repo::scratch_workspace;
use common::run::Run;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn tracked_text() -> String {
    fs::read_to_string(workspace().join("oracle.toml")).unwrap()
}

fn fixture(name: &str) -> PathBuf {
    let dir = xtask::package_dir().join("fixtures/oracle").join(name);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

/// `[variant.<name>]` with one platform's table, at `path` by `md5`; a
/// Linux table names an image and the macOS one the closure the tracked
/// macOS build names, as the grammar asks of each.
fn variant_table(name: &str, key: &str, path: &Path, md5: &str) -> String {
    let image = if key.starts_with("linux") {
        "image = \"sha256:stand-in\"\n".to_owned()
    } else {
        let pin = Pin::parse(&tracked_text()).unwrap();
        let closure = pin
            .variant
            .values()
            .find_map(|v| v.build(Platform::DarwinArm64));
        format!(
            "closure = \"{}\"\n",
            closure.unwrap().provenance.closure.as_ref().unwrap()
        )
    };
    format!(
        "\n[variant.{name}]\nflags = [\"--enable-zend-test\"]\n[variant.{name}.{key}]\npath = {:?}\nmd5 = \"{md5}\"\n{image}",
        path.display().to_string()
    )
}

/// An executable shell file at `path` whose body is `body`, and its md5.
fn shell(path: &Path, body: &str) -> String {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("{:x}", md5::compute(fs::read(path).unwrap()))
}

/// The tracked pin with `[variant.stand-in]` pinned for this machine's
/// platform at a shell file under `dir` whose body is `body`, by its md5
/// or by `md5` where one is given, and the main oracle nowhere: none on a
/// machine of no pinned platform, where no variant can be read.
fn stand_in(dir: &Path, body: &str, md5: Option<&str>) -> Option<(Pin, PathBuf)> {
    let platform = Platform::this()?;
    let path = dir.join("variant/php");
    let own = shell(&path, body);
    let text =
        tracked_text() + &variant_table("stand-in", platform.key(), &path, md5.unwrap_or(&own));
    let mut pin = Pin::parse(&text).unwrap();
    pin.oracle.path = dir.join("no-php-here");
    Some((pin, path))
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

#[test]
fn the_pin_reads_a_variant_and_refuses_a_platform_outside_the_grammar() {
    let text = tracked_text();
    let read = Pin::parse(
        &(text.clone()
            + &variant_table(
                "fixture-variant",
                "darwin-arm64",
                Path::new("/opt/php"),
                "0",
            )),
    )
    .unwrap();
    let variant = &read.variant["fixture-variant"];
    assert_eq!(variant.flags, ["--enable-zend-test"]);
    let build = variant.build(Platform::DarwinArm64).unwrap();
    assert_eq!(
        (build.path.as_path(), build.md5.as_str()),
        (Path::new("/opt/php"), "0")
    );
    assert!(variant.build(Platform::LinuxX86_64).is_none());
    assert_ne!(
        read.digest(),
        Pin::parse(&text).unwrap().digest(),
        "a variant is a value of the pin, so its digest moves with it"
    );

    let err = Pin::parse(
        &(text
            + &variant_table(
                "fixture-variant",
                "darwin-x86_64",
                Path::new("/opt/php"),
                "0",
            )),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("darwin-x86_64"),
        "the offending key is named: {err}"
    );
}

#[test]
fn a_variant_is_the_pin_with_its_build_for_this_platform_in_place_of_the_oracles() {
    let dir = Tree::new("variant-pin").unwrap();
    let Some((pin, path)) = stand_in(&dir, "printf 1", None) else {
        return;
    };
    let tracked = Pin::parse(&tracked_text()).unwrap();
    let variant = pin.variant("stand-in").unwrap();
    assert_eq!(variant.oracle.path, path);
    assert_eq!(
        variant.oracle.md5,
        pin.variant["stand-in"]
            .build(Platform::this().unwrap())
            .unwrap()
            .md5
    );
    assert_eq!(variant.oracle.variant(), Some("stand-in"));
    assert_eq!(variant.oracle.version, pin.oracle.version);
    assert_eq!(variant.oracle.absent(), None);
    assert_eq!(
        variant.recipe.args, pin.recipe.args,
        "the recipe is the main oracle's"
    );
    assert_eq!(variant.recipe.ini, pin.recipe.ini);
    assert_eq!(variant.digest(), pin.digest(), "the digest is the file's");
    assert!(
        variant.sapi.cgi.is_none() && variant.sapi.fpm.is_none() && variant.sapi.phpdbg.is_none(),
        "no host of [sapi] is the variant's"
    );
    // The recipe's environment names the main oracle's binary where the
    // tracked pin does, and the variant's in the variant's pin.
    let names_main: Vec<&String> = tracked
        .recipe
        .env
        .iter()
        .filter(|(_, value)| Path::new(value.as_str()) == tracked.oracle.path)
        .map(|(name, _)| name)
        .collect();
    assert!(
        !names_main.is_empty(),
        "the tracked recipe names its binary"
    );
    let mut main_pin = pin.clone();
    main_pin.oracle.path = tracked.oracle.path.clone();
    let variant = main_pin.variant("stand-in").unwrap();
    for name in names_main {
        assert_eq!(Path::new(&variant.recipe.env[name]), path, "{name}");
    }

    let err = pin.variant("fixture-variant").unwrap_err().to_string();
    assert!(
        err.contains("\"fixture-variant\"") && err.contains("stand-in"),
        "the name asked for and the names the pin has: {err}"
    );
}

#[test]
fn a_variant_pinning_no_build_for_this_platform_is_absent_and_says_which() {
    let Some((platform, other)) = other_platform() else {
        return;
    };
    let text =
        tracked_text() + &variant_table("fixture-variant", other, Path::new("/opt/php"), "0");
    let variant = Pin::parse(&text)
        .unwrap()
        .variant("fixture-variant")
        .unwrap();
    let why = variant.oracle.absent().unwrap();
    assert_eq!(
        why,
        format!(
            "the oracle variant fixture-variant pins no build for {}",
            platform.key()
        )
    );
    let err = variant.oracle.file().unwrap_err().to_string();
    assert_eq!(err, why);
    let err = variant.oracle().asserted().unwrap_err().to_string();
    assert_eq!(
        err, why,
        "nothing is hashed for a build the platform has not"
    );
}

#[test]
fn a_relative_build_path_is_the_pins_directorys_on_load() {
    let Some(platform) = Platform::this() else {
        return;
    };
    let dir = Tree::new("variant-relative").unwrap();
    let text = tracked_text()
        + &variant_table(
            "fixture-variant",
            platform.key(),
            Path::new("build/php"),
            "0",
        );
    fs::write(dir.join("oracle.toml"), &text).unwrap();
    let pin = Pin::load(&dir.join("oracle.toml")).unwrap();
    let variant = pin.variant("fixture-variant").unwrap();
    assert_eq!(variant.oracle.path, dir.join("build/php"));
    assert_eq!(
        variant.oracle.absent(),
        Some(format!(
            "the pinned php of the oracle variant fixture-variant is not at {}",
            dir.join("build/php").display()
        ))
    );
}

/// The negative control of the row (#1890): a `[[crate]]` row naming an
/// oracle variant the pin carries no `[variant.<name>]` for fails that
/// crate's `witness` reading and both its `crosscheck` readings, each
/// naming the variant asked for and the variants the pin has; `phpt`'s is
/// held below, over a tree of its own.
#[test]
fn the_fixture_whose_row_names_a_variant_the_pin_lacks_fails_its_crate() {
    let root = fixture("variant-unknown");
    let manifest = Manifest::load(&root).unwrap();
    let unknown = |message: &str| {
        message.contains("no oracle variant \"no-such-variant\" in the pin; the variants are")
    };

    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &witness::Ask {
            crates: vec!["ext/pcre".to_owned()],
            ..witness::Ask::default()
        },
    )
    .unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(unknown(&reading.message), "{}", reading.message);

    let store = Tree::new("variant-unknown-store").unwrap();
    let readings = crosscheck::run(
        &root,
        &manifest,
        &Registry::default(),
        &Store::at(store.path().to_path_buf()),
        &crosscheck::Ask {
            ext: vec!["pcre".to_owned()],
            ..crosscheck::Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 2, "{readings:#?}");
    for reading in &readings {
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert!(unknown(&reading.message), "{}", reading.message);
    }
}

/// The negative control of the binary (#1890): a crate read under a
/// variant whose binary's md5 is not its pin's refuses `witness` and
/// `crosscheck` as the main oracle's drift does, naming the binary, the
/// md5 the pin says and the variant; `phpt`'s is held below, over a tree
/// of its own.
#[test]
fn the_fixture_whose_variant_binary_differs_from_its_md5_refuses_the_run() {
    if Platform::this().is_none() {
        return;
    }
    let root = fixture("variant-drift");
    let manifest = Manifest::load(&root).unwrap();
    let binary = root.join("php");
    let drifted = |err: &str| {
        err.contains(&binary.display().to_string())
            && err.contains("the pin says 00000000000000000000000000000000")
            && err.contains("of the oracle variant drift")
    };

    let err = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &witness::Ask {
            crates: vec!["ext/pcre".to_owned()],
            ..witness::Ask::default()
        },
    )
    .unwrap_err();
    assert!(drifted(&format!("{err:#}")), "{err:#}");

    let store = Tree::new("variant-drift-store").unwrap();
    let err = crosscheck::run(
        &root,
        &manifest,
        &Registry::default(),
        &Store::at(store.path().to_path_buf()),
        &crosscheck::Ask {
            ext: vec!["pcre".to_owned()],
            ..crosscheck::Ask::default()
        },
    )
    .unwrap_err();
    assert!(drifted(&format!("{err:#}")), "{err:#}");
}

/// A workspace of the test's own: a git repository holding the tracked
/// manifest and pin, and one crate `ext/fixture` whose `src/lib.rs` is
/// `lib`.
fn scratch(name: &str, lib: &str) -> Tree {
    let root = Tree::new(&format!("variant-{name}")).unwrap();
    scratch_workspace(workspace(), &root);
    fs::create_dir_all(root.join("ext/fixture/src")).unwrap();
    fs::write(
        root.join("ext/fixture/Cargo.toml"),
        "[package]\nname = \"php_ext_fixture\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("ext/fixture/src/lib.rs"), lib).unwrap();
    root
}

#[test]
fn witness_reads_a_crate_whose_row_names_a_variant_under_it() {
    let lib =
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn native() {}\n";
    let root = scratch("witness", lib);
    let run = |row: Crate, pin: &Pin| {
        let mut manifest = Manifest::of([row]);
        manifest.pin = Some(pin.clone());
        let readings = witness::run(
            &root,
            &manifest,
            &Registry::default(),
            &witness::Ask {
                crates: vec!["ext/fixture".to_owned()],
                ..witness::Ask::default()
            },
        )
        .unwrap();
        let [reading] = &readings[..] else {
            panic!("{readings:#?}")
        };
        reading.clone()
    };
    let Some((meets, _)) = stand_in(&root.join("meets"), "printf 1", None) else {
        return;
    };
    let reading = run(Crate::named("ext/fixture").with_oracle("stand-in"), &meets);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 1, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("read under the oracle variant stand-in"),
        "{}",
        reading.message
    );

    // A variant that prints what the fence does not pin fails the fence:
    // the oracle's side is the variant's.
    let (unmet, _) = stand_in(&root.join("unmet"), "printf 2", None).unwrap();
    let reading = run(Crate::named("ext/fixture").with_oracle("stand-in"), &unmet);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 1, "{reading:#?}");

    // The row without the key is read under the main oracle, which is
    // nowhere here: nothing runs.
    let reading = run(Crate::named("ext/fixture"), &meets);
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("no-php-here"),
        "{}",
        reading.message
    );
}

#[test]
fn witness_under_a_variant_with_no_build_for_this_platform_is_a_skip_naming_it() {
    let Some((platform, other)) = other_platform() else {
        return;
    };
    let lib =
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn native() {}\n";
    let root = scratch("witness-unpinned", lib);
    let mut manifest = Manifest::of([Crate::named("ext/fixture").with_oracle("fixture-variant")]);
    manifest.pin = Some(
        Pin::parse(
            &(tracked_text()
                + &variant_table("fixture-variant", other, Path::new("/opt/php"), "0")),
        )
        .unwrap(),
    );
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &witness::Ask {
            crates: vec!["ext/fixture".to_owned()],
            ..witness::Ask::default()
        },
    )
    .unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "not_run"), 1, "{reading:#?}");
    assert!(
        reading.message.contains(&format!(
            "the oracle variant fixture-variant pins no build for {}",
            platform.key()
        )),
        "{}",
        reading.message
    );
}

#[test]
fn crosscheck_renders_an_extension_whose_row_names_a_variant_under_it() {
    let root = Tree::new("variant-crosscheck").unwrap();
    scratch_workspace(workspace(), &root);
    let fixtures = xtask::package_dir().join("fixtures/crosscheck");
    let body = format!(
        "for arg in \"$@\"; do\n  case \"$arg\" in\n    --re) exec cat '{}' ;;\n    --ri) exec cat '{}' ;;\n  esac\ndone\nexit 1",
        fixtures.join("oracle.re").display(),
        fixtures.join("oracle.ri").display()
    );
    let Some((pin, _)) = stand_in(&root, &body, None) else {
        return;
    };
    let renderings = root.join("renderings");
    fs::create_dir_all(&renderings).unwrap();
    fs::copy(fixtures.join("oracle.re"), renderings.join("ctype.re")).unwrap();
    fs::copy(fixtures.join("oracle.ri"), renderings.join("ctype.ri")).unwrap();
    let store = Store::at(root.join("target/harness"));
    let ask = crosscheck::Ask {
        ext: vec!["ctype".to_owned()],
        renderings: Some(renderings),
        ..crosscheck::Ask::default()
    };
    let mut manifest = Manifest::named("ext/ctype");
    manifest.pin = Some(pin);
    let run = |manifest: &Manifest| {
        crosscheck::run(&root, manifest, &Registry::default(), &store, &ask).unwrap()
    };

    let mut varied = manifest.clone();
    varied
        .crates
        .iter_mut()
        .find(|krate| krate.name == "ext/ctype")
        .unwrap()
        .oracle = Some("stand-in".to_owned());
    let readings = run(&varied);
    assert_eq!(readings.len(), 2, "{readings:#?}");
    for reading in &readings {
        assert_eq!(reading.status, Status::Pass, "{reading:#?}");
        assert!(
            reading
                .message
                .contains("read under the oracle variant stand-in"),
            "{}",
            reading.message
        );
    }
    let md5 = varied
        .pin()
        .unwrap()
        .variant("stand-in")
        .unwrap()
        .oracle
        .md5;
    for column in [Column::Re, Column::Ri] {
        let kept = crosscheck::kept(&store, &md5, "ctype", column);
        assert!(
            kept.is_file(),
            "kept under the variant's md5: {}",
            kept.display()
        );
    }

    // The row without the key is rendered under the main oracle, which is
    // nowhere here.
    for reading in run(&manifest) {
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains("no-php-here"),
            "{}",
            reading.message
        );
    }
}

/// A workspace and a corpus for `phpt`: `ext/fixture` whose subset holds
/// one test that the stand-in variant, run as the binary under test,
/// meets; the manifest's one row names the variant where `varied`.
fn phpt_scratch(name: &str, md5: Option<&str>, varied: bool) -> Option<(Tree, Manifest, PathBuf)> {
    let root = scratch(name, "");
    let corpus = root.join("corpus");
    let tests = corpus.join("ext/fixture/tests");
    fs::create_dir_all(&tests).unwrap();
    fs::write(
        tests.join("variant.phpt"),
        "--TEST--\nUnder the variant\n--FILE--\n<?php echo 'variant';\n--EXPECT--\nvariant\n",
    )
    .unwrap();
    let (pin, binary) = stand_in(&root, "printf variant", md5)?;
    let row = Crate::named("ext/fixture");
    let row = if varied {
        row.with_oracle("stand-in")
    } else {
        row
    };
    let mut manifest = Manifest::of([row]);
    manifest.pin = Some(pin.on_this_machine(Some(corpus)));
    Some((root, manifest, binary))
}

fn phpt_run(root: &Path, manifest: &Manifest, engine: &Path) -> Reading {
    let locks = Tree::new("variant-phpt-locks").unwrap();
    let ask = phpt::Ask {
        subset: Some("ext/fixture".to_owned()),
        engine: phpt::Engine::Named(engine.to_path_buf()),
        locks: Some(locks.path().to_path_buf()),
        ..phpt::Ask::default()
    };
    let readings = phpt::run(root, manifest, &Registry::default(), &ask).unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    reading.clone()
}

#[test]
fn phpt_runs_a_subset_whose_row_names_a_variant_under_it() {
    let Some((root, manifest, binary)) = phpt_scratch("phpt", None, true) else {
        return;
    };
    // The variant's own binary as the binary under test is the runner's
    // control under the variant's pin, which is how the subset's target
    // under the variant is kept: `pin-<tag>-<the variant's md5>`.
    let reading = phpt_run(&root, &manifest, &binary);
    assert_eq!(reading.scope, "ext/fixture@control", "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 1, "{reading:#?}");
    let md5 = manifest
        .pin()
        .unwrap()
        .variant("stand-in")
        .unwrap()
        .oracle
        .md5;
    assert!(
        reading.message.contains(&format!(
            "kept as the subset's target under pin-{}-{md5}",
            manifest.pin().unwrap().source.tag
        )),
        "{}",
        reading.message
    );
    assert!(
        reading
            .message
            .contains("read under the oracle variant stand-in"),
        "{}",
        reading.message
    );

    // The row without the key runs under the main oracle, which is
    // nowhere here: nothing runs, and the reading says why.
    let (root, manifest, binary) = phpt_scratch("phpt-main", None, false).unwrap();
    let reading = phpt_run(&root, &manifest, &binary);
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 0, "{reading:#?}");
    assert!(
        reading.message.contains("no-php-here"),
        "{}",
        reading.message
    );
}

/// `--unkept` under a variant (#1830, #1782): the subset's target is kept
/// and found under the variant's pin, so run again under the variant's own
/// binary it stands and is not run; and the main php, which is no control
/// of the variant's subset, leaves it rather than running it.
#[test]
fn phpt_unkept_holds_a_variants_subset_to_the_target_kept_under_the_variants_pin() {
    let Some((root, manifest, binary)) = phpt_scratch("phpt-unkept", None, true) else {
        return;
    };
    let locks = Tree::new("variant-unkept-locks").unwrap();
    let run = |engine: &Path| {
        let ask = phpt::Ask {
            subset: Some("ext/fixture".to_owned()),
            engine: phpt::Engine::Named(engine.to_path_buf()),
            locks: Some(locks.path().to_path_buf()),
            unkept: true,
            ..phpt::Ask::default()
        };
        phpt::run(&root, &manifest, &Registry::default(), &ask)
    };
    let readings = run(&binary).unwrap();
    assert_eq!(
        readings.len(),
        1,
        "no target kept, so it runs: {readings:#?}"
    );
    let md5 = manifest
        .pin()
        .unwrap()
        .variant("stand-in")
        .unwrap()
        .oracle
        .md5;
    let kept = root.join(format!(
        "target/harness/phpt-runs/pin-{}-{md5}/ext/fixture.json",
        manifest.pin().unwrap().source.tag
    ));
    assert!(
        kept.is_file(),
        "kept under the variant's pin: {}",
        kept.display()
    );

    let readings = run(&binary).unwrap();
    assert!(
        readings.is_empty(),
        "the variant's target stands: {readings:#?}"
    );

    // A binary that is no subset's control is refused, the main php here
    // among them, being nowhere.
    let main = root.join("main-php");
    fs::write(&main, "#!/bin/sh\n").unwrap();
    let refused = run(&main).expect_err("no subset's control");
    assert!(format!("{refused:#}").contains("--unkept"), "{refused:#}");
}

/// The runner's control with no php named (#1970): `--control --unkept`
/// runs a variant's subset under the variant's own php and keeps its target
/// under the variant's pin, which then stands; a subset of the main oracle,
/// whose php is not here, runs nothing and says why.
#[test]
fn phpt_control_keeps_a_variants_target_under_its_own_pin_with_no_php_named() {
    let Some((root, manifest, _)) = phpt_scratch("phpt-control", None, true) else {
        return;
    };
    let locks = Tree::new("variant-control-locks").unwrap();
    let ask = phpt::Ask {
        subset: Some("ext/fixture".to_owned()),
        engine: phpt::Engine::Control,
        unkept: true,
        locks: Some(locks.path().to_path_buf()),
        ..phpt::Ask::default()
    };
    let run = |manifest: &Manifest| phpt::run(&root, manifest, &Registry::default(), &ask);
    let readings = run(&manifest).unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.scope, "ext/fixture@control", "{reading:#?}");
    assert_eq!(count(reading, "passed"), 1, "{reading:#?}");
    let md5 = manifest
        .pin()
        .unwrap()
        .variant("stand-in")
        .unwrap()
        .oracle
        .md5;
    let kept = root.join(format!(
        "target/harness/phpt-runs/pin-{}-{md5}/ext/fixture.json",
        manifest.pin().unwrap().source.tag
    ));
    assert!(kept.is_file(), "{}", kept.display());
    let readings = run(&manifest).unwrap();
    assert!(readings.is_empty(), "the target stands: {readings:#?}");

    let mut main = manifest.clone();
    main.crates[0].oracle = None;
    let readings = run(&main).unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.scope, "ext/fixture@control", "{reading:#?}");
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("no-php-here"),
        "{}",
        reading.message
    );
}

/// The oracle job runs the control with `--control`, so dashboard.yml
/// names no php for it and no variant: each subset's own pinned php is the
/// harness's to find (#1970).
#[test]
fn the_oracle_jobs_control_names_no_php_and_no_variant() {
    let text = fs::read_to_string(workspace().join(".github/workflows/dashboard.yml")).unwrap();
    let commands: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    assert!(
        commands.contains(&"set -- --control"),
        "the control step asks for the control"
    );
    for line in commands {
        assert!(
            !line.contains("--engine") && !line.contains(".binary.path"),
            "{line}"
        );
    }
    let pin = Pin::parse(&tracked_text()).unwrap();
    for name in pin.variant.keys() {
        assert!(!text.contains(name.as_str()), "dashboard.yml names {name}");
    }
}

/// `phpt`'s half of the binary's negative control: the subset of a crate
/// read under a variant whose binary is not its md5 is refused, a failing
/// reading naming the drift, as the main oracle's is.
#[test]
fn phpt_refuses_a_subset_under_a_variant_whose_binary_differs_from_its_md5() {
    let wrong = "0".repeat(32);
    let Some((root, manifest, binary)) = phpt_scratch("phpt-drift", Some(&wrong), true) else {
        return;
    };
    let reading = phpt_run(&root, &manifest, &binary);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains(&format!("the pin says {wrong}"))
            && reading.message.contains("of the oracle variant stand-in"),
        "{}",
        reading.message
    );
}

/// `phpt`'s half of the row's negative control: the subset of a crate whose
/// row names a variant the pin lacks is refused, a failing reading naming
/// the variant, and the lane writes it beside the others.
#[test]
fn phpt_refuses_a_subset_whose_row_names_a_variant_the_pin_lacks() {
    let Some((root, mut manifest, binary)) = phpt_scratch("phpt-unknown", None, true) else {
        return;
    };
    manifest.crates[0].oracle = Some("no-such-variant".to_owned());
    let reading = phpt_run(&root, &manifest, &binary);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains(
            "no oracle variant \"no-such-variant\" in the pin; the variants are stand-in"
        ),
        "{}",
        reading.message
    );
}

/// `xtask phpt --subset ext/fixture` with no `--engine`, in a workspace
/// whose row names the stand-in variant and the feature `fixture`, which
/// no binary under test turns on there (#2029): cargo is the shell file
/// `build` writes, and `target/release-da/php`, the default build, is a
/// decoy that misses the test's expectation. The subset's reading.
fn phpt_featured(name: &str, build: &str) -> Option<(Tree, Reading)> {
    let root = scratch(name, "");
    let tests = root.join("corpus/ext/fixture/tests");
    fs::create_dir_all(&tests).unwrap();
    fs::write(
        tests.join("variant.phpt"),
        "--TEST--\nOn the engine with the feature\n--FILE--\n<?php echo 'variant';\n--EXPECT--\nvariant\n",
    )
    .unwrap();
    let platform = Platform::this()?;
    let php = root.join("variant/php");
    let md5 = shell(&php, "printf variant");
    let pin = root.join("oracle.toml");
    let text = fs::read_to_string(&pin).unwrap();
    fs::write(
        &pin,
        text + &variant_table("stand-in", platform.key(), &php, &md5),
    )
    .unwrap();
    Tracked::of(&root)
        .with_crate("ext/fixture", |row| {
            row.budget = 100;
            row.feature = Some("fixture".to_owned());
            row.oracle = Some("stand-in".to_owned());
        })
        .write(&root);
    shell(&root.join("target/release-da/php"), "printf default");
    let cargo = root.join("bin/cargo");
    shell(&cargo, build);
    let temp = root.join("tmp");
    fs::create_dir_all(&temp).unwrap();
    let ran = Run::at(&root)
        .args(["phpt", "--subset", "ext/fixture", "--jobs", "1"])
        .env("CARGO", &cargo)
        .env("TMPDIR", &temp)
        .env("TEST_PHP_SRCDIR", root.join("corpus"))
        .go();
    let reading = Store::of_workspace(&root)
        .read("phpt", "ext/fixture")
        .unwrap()
        .unwrap_or_else(|| panic!("no reading: {ran}"));
    Some((root, reading))
}

/// The engine the harness builds with the row's feature, into a target
/// directory of its own, is the one the subset runs on, and the default
/// build is left where it stands.
#[test]
fn phpt_runs_a_variants_subset_on_the_engine_built_with_its_rows_feature() {
    let build = "while [ $# -gt 0 ]; do\n  case \"$1\" in\n    --target-dir) dir=$2; shift ;;\n    --features) features=$2; shift ;;\n  esac\n  shift\ndone\n[ \"$features\" = php_bundle/fixture ] || exit 3\nmkdir -p \"$dir/release-da\" && printf '#!/bin/sh\\nprintf variant\\n' > \"$dir/release-da/php\" && chmod +x \"$dir/release-da/php\"";
    let Some((root, reading)) = phpt_featured("phpt-featured", build) else {
        return;
    };
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 1, "{reading:#?}");
    let engine = root.join("target/featured/fixture/release-da/php");
    assert!(
        reading
            .message
            .contains(&format!("run by {}", engine.display())),
        "{}",
        reading.message
    );
}

/// The negative control: where the feature build is missing, cargo
/// failing it, the subset is refused naming the build, and neither runs
/// on the default build nor skips as an engine not yet built would.
#[test]
fn phpt_refuses_a_variants_subset_whose_feature_build_is_missing() {
    let Some((_root, reading)) = phpt_featured("phpt-featured-missing", "exit 101") else {
        return;
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "refused"), 1, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("the engine with php_bundle/fixture is not built"),
        "{}",
        reading.message
    );
}

/// The key of a pinned platform other than this machine's, none on a
/// machine of no pinned platform.
fn other_platform() -> Option<(Platform, &'static str)> {
    let platform = Platform::this()?;
    let other = ["darwin-arm64", "linux-x86_64", "linux-aarch64"]
        .into_iter()
        .find(|key| *key != platform.key())?;
    Some((platform, other))
}

#[test]
fn crosscheck_under_a_variant_with_no_build_for_this_platform_is_a_skip_naming_it() {
    let Some((platform, other)) = other_platform() else {
        return;
    };
    let root = Tree::new("variant-crosscheck-unpinned").unwrap();
    scratch_workspace(workspace(), &root);
    let mut manifest = Manifest::named("ext/ctype");
    manifest.pin = Some(
        Pin::parse(
            &(tracked_text()
                + &variant_table("fixture-variant", other, Path::new("/opt/php"), "0")),
        )
        .unwrap(),
    );
    manifest
        .crates
        .iter_mut()
        .find(|krate| krate.name == "ext/ctype")
        .unwrap()
        .oracle = Some("fixture-variant".to_owned());
    let store = Store::at(root.join("target/harness"));
    let readings = crosscheck::run(
        &root,
        &manifest,
        &Registry::default(),
        &store,
        &crosscheck::Ask {
            ext: vec!["ctype".to_owned()],
            ..crosscheck::Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 2, "{readings:#?}");
    for reading in &readings {
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains(&format!(
                "the oracle variant fixture-variant pins no build for {}",
                platform.key()
            )),
            "{}",
            reading.message
        );
    }
}

#[test]
fn phpt_under_a_variant_with_no_build_for_this_platform_is_a_skip_naming_it() {
    let Some((platform, other)) = other_platform() else {
        return;
    };
    let (root, mut manifest, binary) = phpt_scratch("phpt-unpinned", None, true).unwrap();
    let tree = manifest.pin().unwrap().source.tree().unwrap();
    manifest.pin = Pin::parse(
        &(tracked_text() + &variant_table("stand-in", other, Path::new("/opt/php"), "0")),
    )
    .unwrap()
    .on_this_machine(Some(tree))
    .into();
    let reading = phpt_run(&root, &manifest, &binary);
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(&reading, "passed"), 0, "{reading:#?}");
    assert!(
        reading.message.contains(&format!(
            "the oracle variant stand-in pins no build for {}",
            platform.key()
        )),
        "{}",
        reading.message
    );
}

/// The negative controls of the closure (#2935), one fixture each under
/// `xtask/fixtures/oracle/closure/`: a macOS build naming no closure, a
/// Linux build naming one, and a macOS build made against a `[closure]`
/// the pin has moved past. Each pin is refused on load, on any machine,
/// naming the variant's table and the person's re-pin.
#[test]
fn the_fixtures_whose_build_the_closure_does_not_hold_are_refused_on_load() {
    let moved = "was made against the closure sha256:f3f950ce43447203da79ea7a1a4391bdbdc1963bf63528f58b46da443e68e2b8, and the pin's [closure] is sha256:f22ee36557db677d4e720a3e315fa3aa26230a9d9e6c0489eee4826a4bae71e4, so its build predates the closure";
    for (name, key, refusal) in [
        (
            "unnamed",
            "darwin-arm64",
            "names no closure, so its build predates the pin's [closure]",
        ),
        ("linux", "linux-x86_64", "names the closure sha256:f22ee365"),
        ("moved", "darwin-arm64", moved),
    ] {
        let path = fixture("closure").join(format!("{name}.toml"));
        let err = format!("{:#}", Pin::load(&path).unwrap_err());
        let at = format!("[variant.closure.{key}] ");
        let repin = format!("`scripts/oracle-build.py build {key} closure` is a person's act");
        assert!(
            err.contains(&(at + refusal)) && err.contains(&repin),
            "{name}: {err}"
        );
    }
}

/// The recipe prints the closure digest a darwin-arm64 build is made
/// against (#2935) as xtask makes it: `scripts/oracle-build.py closure`
/// over the tracked pin is the digest its macOS build names, which the
/// pin's grammar holds to `[closure]`'s. Run where the macOS build is
/// made, whose runner installs the recipe's Python.
#[cfg(target_os = "macos")]
#[test]
fn the_recipe_prints_the_closure_digest_the_tracked_macos_build_names() {
    let out = std::process::Command::new("python3")
        .arg("-I")
        .arg(workspace().join("scripts/oracle-build.py"))
        .arg("closure")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let pin = Pin::parse(&tracked_text()).unwrap();
    let named = (pin.variant.values()).filter_map(|variant| {
        variant
            .build(Platform::DarwinArm64)?
            .provenance
            .closure
            .clone()
    });
    let named: Vec<String> = named.map(|digest| digest.to_string()).collect();
    assert!(!named.is_empty(), "the tracked pin names a macOS build");
    for digest in named {
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), digest);
    }
}

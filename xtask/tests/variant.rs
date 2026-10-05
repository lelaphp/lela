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

mod common;
use common::scratch_workspace;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn tracked_text() -> String {
    fs::read_to_string(workspace().join("oracle.toml")).unwrap()
}

fn fixture(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/oracle")
        .join(name);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

/// `[variant.<name>]` with one platform's table, at `path` by `md5`; a
/// Linux table names an image, as the grammar asks of it.
fn variant_table(name: &str, key: &str, path: &Path, md5: &str) -> String {
    let image = if key.starts_with("linux") {
        "image = \"sha256:stand-in\"\n"
    } else {
        ""
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
        &(text.clone() + &variant_table("zend-test", "darwin-arm64", Path::new("/opt/php"), "0")),
    )
    .unwrap();
    let variant = &read.variant["zend-test"];
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
        &(text + &variant_table("zend-test", "darwin-x86_64", Path::new("/opt/php"), "0")),
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

    let err = pin.variant("zend-test").unwrap_err().to_string();
    assert!(
        err.contains("\"zend-test\"") && err.contains("stand-in"),
        "the name asked for and the names the pin has: {err}"
    );
}

#[test]
fn a_variant_pinning_no_build_for_this_platform_is_absent_and_says_which() {
    let Some(platform) = Platform::this() else {
        return;
    };
    let other = ["darwin-arm64", "linux-x86_64", "linux-aarch64"]
        .into_iter()
        .find(|key| *key != platform.key())
        .unwrap();
    let text = tracked_text() + &variant_table("zend-test", other, Path::new("/opt/php"), "0");
    let variant = Pin::parse(&text).unwrap().variant("zend-test").unwrap();
    let why = variant.oracle.absent().unwrap();
    assert_eq!(
        why,
        format!(
            "the oracle variant zend-test pins no build for {}",
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
    let text =
        tracked_text() + &variant_table("zend-test", platform.key(), Path::new("build/php"), "0");
    fs::write(dir.join("oracle.toml"), &text).unwrap();
    let pin = Pin::load(&dir.join("oracle.toml")).unwrap();
    let variant = pin.variant("zend-test").unwrap();
    assert_eq!(variant.oracle.path, dir.join("build/php"));
    assert_eq!(
        variant.oracle.absent(),
        Some(format!(
            "the pinned php of the oracle variant zend-test is not at {}",
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
    scratch_workspace(workspace(), &root).unwrap();
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
        manifest.pin = pin.clone();
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
    let Some(platform) = Platform::this() else {
        return;
    };
    let lib =
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn native() {}\n";
    let root = scratch("witness-unpinned", lib);
    let other = ["darwin-arm64", "linux-x86_64", "linux-aarch64"]
        .into_iter()
        .find(|key| *key != platform.key())
        .unwrap();
    let mut manifest = Manifest::of([Crate::named("ext/fixture").with_oracle("zend-test")]);
    manifest.pin = Pin::parse(
        &(tracked_text() + &variant_table("zend-test", other, Path::new("/opt/php"), "0")),
    )
    .unwrap();
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
            "the oracle variant zend-test pins no build for {}",
            platform.key()
        )),
        "{}",
        reading.message
    );
}

#[test]
fn crosscheck_renders_an_extension_whose_row_names_a_variant_under_it() {
    let root = Tree::new("variant-crosscheck").unwrap();
    scratch_workspace(workspace(), &root).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/crosscheck");
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
    };
    let mut manifest = Manifest::load(workspace()).unwrap();
    manifest.pin = pin;
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
                .contains("under the oracle variant `stand-in`"),
            "{}",
            reading.message
        );
    }
    let md5 = varied.pin.variant("stand-in").unwrap().oracle.md5;
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
    manifest.pin = pin.on_this_machine(Some(corpus));
    Some((root, manifest, binary))
}

fn phpt_run(root: &Path, manifest: &Manifest, engine: &Path) -> Reading {
    let locks = Tree::new("variant-phpt-locks").unwrap();
    let ask = phpt::Ask {
        subset: Some("ext/fixture".to_owned()),
        engine: Some(engine.to_path_buf()),
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
    let md5 = manifest.pin.variant("stand-in").unwrap().oracle.md5;
    assert!(
        reading.message.contains(&format!(
            "kept as the subset's target under pin-{}-{md5}",
            manifest.pin.source.tag
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

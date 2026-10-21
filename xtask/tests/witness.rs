//! `xtask witness`, the witness sensor (design §8; harness-design.md §4.3,
//! §4.5 conditions 1 and 2, §8): the fence pairs of a crate's rustdoc, each
//! snippet run under the oracle's recipe and held to what its fences pin,
//! and the kernels and natives without one. Every test here goes through
//! the instrument's `run` and reads what it wrote; how one witness is
//! judged, the notes and the digest, is held beside the judge.
//!
//! Every snippet here is served by the engine linked into the harness,
//! which is what the lane serves every witness through and the one thing a
//! binary in the tree does not change (harness §7, revision 48; #401,
//! #119). The tests that run one return when the pinned oracle is not on
//! this machine, unless the machine promised it (#38); the rest, the
//! coverage half and the readings of a run with nothing to run, run
//! anywhere.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::manifest::{Crate, Manifest};
use xtask::oracle::Promise;
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::witness::{self, Accept, Ask, Scope};

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
    pub mod run;
}
use common::manifest::{Tracked, pin};
use common::registry::registry;
use common::repo::{Repo, scratch_workspace};
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The tracked manifest, for the tests in [`tracked`] alone.
fn tracked_manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// The negative-control fixture of one manifest rule (harness §4.1).
fn fixture(rule: &str) -> PathBuf {
    fixture_at("rules", rule)
}

/// A negative-control fixture by the two names its path carries: the
/// instrument's, or `rules` for one of a manifest rule's.
fn fixture_at(kind: &str, name: &str) -> PathBuf {
    let dir = xtask::package_dir().join("fixtures").join(kind).join(name);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = pin();
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// What php writes on the first stream for an uncaught error at line 2,
/// which the engine writes with it, opted in to the status `--accept`
/// captures. The path is `%s`, since the oracle runs the snippet in a
/// scratch directory and the linked engine as `witness.php` (#293).
const FATAL: &str = "/// ```expect exit\n/// Fatal error: Uncaught Error: Call to undefined function nope() in %s:2\n/// Stack trace:\n/// #0 {main}\n///   thrown in %s on line 2\n/// ```\n";

/// A block whose snippet prints `1` and whose fences pin what `fences` says.
fn block(fences: &str) -> String {
    format!("/// ```php\n/// echo 1;\n/// ```\n{fences}pub fn native() {{}}\n")
}

/// A workspace of this test's own: a git repository holding the tracked
/// manifest, and one crate `ext/fixture` whose `src/lib.rs` is `text`. No
/// binary stands in it; the one test that lays one down is the one that
/// holds the lane to serving its witnesses without it.
fn scratch(name: &str, text: &str) -> Tree {
    let root = Tree::new(&format!("witness-{name}")).unwrap();
    scratch_workspace(workspace(), &root);
    fs::create_dir_all(root.join("ext/fixture/src")).unwrap();
    fs::write(
        root.join("ext/fixture/Cargo.toml"),
        "[package]\nname = \"php_ext_fixture\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("ext/fixture/src/lib.rs"), text).unwrap();
    root
}

/// A shell file at `target/debug/php` that writes bytes no fence pins and
/// ends on 7: what the lane would read if it resolved a binary, and what
/// it must not read.
fn a_binary_where_the_engine_s_would_be(root: &Path) {
    let dir = root.join("target/debug");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("php");
    fs::write(&file, "#!/bin/sh\necho 'not the engine'\nexit 7\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// The manifest a run over the scratch workspace reads: one crate row for
/// the fixture under the tracked pin, and no rule, so nothing is counted
/// as a kernel or native that needs a witness.
fn fixture_manifest() -> Manifest {
    let mut manifest = Manifest::named("ext/fixture");
    manifest.pin = pin();
    manifest
}

/// The one reading of `ext/fixture` over `root`.
fn run(root: &Path, registry: &Registry, accept: Accept) -> Reading {
    let readings = witness::run(
        root,
        &fixture_manifest(),
        registry,
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            accept,
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    readings.into_iter().next().unwrap()
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// The divergence key of a witness (harness §4.3): `witness:` and git's
/// blob id of the snippet, as `git hash-object` gives it.
fn witness_id(snippet: &str) -> String {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(snippet.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    format!("witness:{}", String::from_utf8(out.stdout).unwrap().trim())
}

#[test]
fn a_witness_whose_bytes_are_the_oracle_s_passes() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("pass", &block("/// ```expect\n/// 1\n/// ```\n"));
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert_eq!(count(&reading, "missing"), 0);
}

/// Every crate's fences are served on one pool (#2212), and each crate's
/// reading is the one a run over it alone gives: of two crates, one whose
/// fence the oracle meets and one whose fence it does not, a run over
/// both reads each as the run over each alone does, in the manifest's
/// order.
#[test]
fn one_pool_over_two_crates_reads_each_as_a_run_over_it_alone() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("two-crates", &block("/// ```expect\n/// 1\n/// ```\n"));
    fs::create_dir_all(root.join("ext/second/src")).unwrap();
    fs::write(
        root.join("ext/second/Cargo.toml"),
        "[package]\nname = \"php_ext_second\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("ext/second/src/lib.rs"),
        block("/// ```expect\n/// 2\n/// ```\n"),
    )
    .unwrap();
    let mut manifest = fixture_manifest();
    manifest.crates.push(Crate::named("ext/second"));
    let over = |crates: &[&str]| {
        witness::run(
            &root,
            &manifest,
            &Registry::default(),
            &Ask {
                crates: crates.iter().map(|c| (*c).to_owned()).collect(),
                ..Ask::default()
            },
        )
        .unwrap()
    };
    let verdict = |r: &Reading| {
        (
            r.scope.clone(),
            r.status,
            r.numbers.clone(),
            r.message.clone(),
        )
    };
    let both: Vec<_> = over(&["ext/fixture", "ext/second"])
        .iter()
        .map(verdict)
        .collect();
    let alone: Vec<_> = ["ext/fixture", "ext/second"]
        .iter()
        .flat_map(|c| over(&[c]))
        .map(|r| verdict(&r))
        .collect();
    assert_eq!(both, alone);
    assert_eq!(both[0].0, "ext/fixture");
    assert_eq!(both[0].1, Status::Pass, "{both:#?}");
    assert_eq!(both[1].0, "ext/second");
    assert_eq!(both[1].1, Status::Fail, "{both:#?}");
}

/// A fence that prints `1` on `name`, opted in to the status `--accept`
/// captures.
fn bare(name: &str) -> String {
    format!(
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect exit\n/// 1\n/// ```\npub fn {name}() {{}}\n"
    )
}

/// `--file` and `--item` narrow the run and `--accept` to the fences they
/// name (#1146): of two bare fences, one in each file of the crate, a
/// narrowed accept pins the named one and leaves the other's line as it
/// stood, and the run judges one witness and counts no coverage.
#[test]
fn file_and_item_narrow_the_run_and_accept_to_the_named_fences() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("narrow", &bare("one"));
    fs::write(root.join("ext/fixture/src/other.rs"), bare("two")).unwrap();
    let narrowed = |scope: Scope| {
        let readings = witness::run(
            &root,
            &fixture_manifest(),
            &registry(""),
            &Ask {
                crates: vec!["ext/fixture".to_owned()],
                accept: Accept::Status,
                scope,
                ..Ask::default()
            },
        )
        .unwrap();
        let reading = readings.into_iter().next().unwrap();
        assert_eq!(count(&reading, "run"), 1, "{reading:#?}");
        assert_eq!(count(&reading, "accepted"), 1, "{reading:#?}");
        assert_eq!(count(&reading, "subjects"), 0, "{reading:#?}");
    };
    let pinned = |file: &str| {
        !fs::read_to_string(root.join("ext/fixture/src").join(file))
            .unwrap()
            .contains("```expect exit\n")
    };
    narrowed(Scope::File(PathBuf::from("src/other.rs")));
    assert!(pinned("other.rs") && !pinned("lib.rs"));
    narrowed(Scope::Item("one".to_owned()));
    assert!(pinned("lib.rs"));
}

/// A narrowed run covers fewer fences than the crate, so what it judged
/// is printed and no reading is written (#1146): the command over the
/// fixture's one item leaves the store without a witness reading of the
/// crate, and an item no fence stands on is an error naming it.
#[test]
fn a_narrowed_run_writes_no_reading() {
    let root = scratch("narrow-store", &bare("one"));
    let pin = "oracle.toml";
    fs::write(
        root.join("seams.toml"),
        format!(
            "[oracle]\npin = \"{pin}\"\n[budget]\n{}[[crate]]\nname = \"ext/fixture\"\nbudget = 1000\n",
            ["total", "new", "docs", "harness", "lock_packages", "clean_build_release_s", "api_items", "bindings"]
                .map(|key| format!("{key} = 1000\n"))
                .concat()
        ),
    )
    .unwrap();
    let narrowed = |item: &str| {
        xtask(
            &root,
            &["witness", "--crate", "ext/fixture", "--item", item],
        )
    };
    let ran = narrowed("one");
    assert!(
        ran.stdout
            .contains("witness: a run narrowed to the fences on one writes no reading"),
        "{ran}"
    );
    let store = Store::of_workspace(root.path());
    assert!(store.read("witness", "ext/fixture").unwrap().is_none());

    let stderr = narrowed("nope").failed();
    assert!(stderr.contains("no fence stands on nope"), "{stderr}");
    assert!(store.read("witness", "ext/fixture").unwrap().is_none());
}

#[test]
fn a_witness_that_differs_is_a_difference_the_registry_can_accept() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("differs", &block("/// ```expect\n/// 2\n/// ```\n"));
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 1);
    assert!(
        reading.message.contains("1 of 1 witnesses missed")
            && reading.message.contains("ext/fixture/src/lib.rs:1"),
        "{}",
        reading.message
    );

    // An accepted row names one side of a difference between the engine
    // and the oracle, so it never accepts a fence both runs miss (#783).
    let accepted = |snippet: &str| {
        format!(
            "[[divergence]]\nid = \"{}\"\nkind = \"witness\"\nreason = \"A row a person wrote.\"\nstatus = \"accepted\"\nsince = \"2026-09-09\"\n",
            witness_id(snippet)
        )
    };
    let reading = run(&root, &registry(&accepted("echo 1;")), Accept::Nothing);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");

    // A fence the oracle meets and the engine misses: the bundle
    // `php-linked` serves carries zend_test and the oracle does not.
    // `__FILE__` was the difference once, until the engine compiled a
    // served source under its name expanded, as the oracle does (#1569),
    // and pcntl after it, until the linked engine was the CLI's (#2166).
    let root = scratch(
        "diverged",
        "/// ```php\n/// var_dump(extension_loaded('zend_test'));\n/// ```\n/// ```expect\n/// bool(false)\n/// ```\npub fn native() {}\n",
    );
    assert_eq!(
        run(&root, &registry(""), Accept::Nothing).status,
        Status::Fail
    );
    let reading = run(
        &root,
        &registry(&accepted("var_dump(extension_loaded('zend_test'));")),
        Accept::Nothing,
    );
    assert_eq!(
        reading.status,
        Status::Pass,
        "an accepted row is not a failure: {reading:#?}"
    );
    assert_eq!(count(&reading, "diverged"), 1);
    assert_eq!(count(&reading, "failed"), 0);
}

#[test]
fn a_bare_exit_fails_the_run_and_accept_is_what_writes_it() {
    if oracle().is_none() {
        return;
    }
    // php's uncaught error, which the engine writes byte for byte and both
    // it and the oracle end 255 on: a status of the snippet's own, where
    // `exit(3)` is a construct the compiler does not read yet and so is
    // not one the run of record can serve.
    let text = format!("/// ```php\n/// nope();\n/// ```\n{FATAL}pub fn native() {{}}\n");
    let text = text.as_str();
    let root = scratch("accept", text);
    let file = root.join("ext/fixture/src/lib.rs");

    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "unpinned"), 1);
    assert_eq!(count(&reading, "accepted"), 0);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        text,
        "a run that does not accept writes nothing"
    );

    let reading = run(&root, &registry(""), Accept::Status);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "accepted"), 1);
    let written = fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("```expect exit=255"),
        "the oracle's status is on the fence line: {written}"
    );
    // Nothing else of the file moved.
    assert_eq!(written.lines().count(), text.lines().count());
    assert!(written.contains("/// nope();"), "{written}");
    // Stamped over the tree `--accept` left, a snapshot taken after it
    // wrote, so `done` finds the reading fresh for the line it pinned
    // (#2163).
    let now = xtask::key::Snapshot::disk(&root).unwrap();
    assert_eq!(reading.key(), now.key(Path::new("ext/fixture")).unwrap());

    // The witness now pins the status, so a second run accepts nothing.
    let again = run(&root, &registry(""), Accept::Status);
    assert_eq!(again.status, Status::Pass, "{again:#?}");
    assert_eq!(count(&again, "accepted"), 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), written);
}

#[test]
fn accept_writes_the_bare_opt_in_alone_and_never_repins_a_status() {
    if oracle().is_none() {
        return;
    }
    // A fence that pins a status the oracle no longer gives, and one that
    // pins none while the oracle exits 3: both are differences for a person
    // to read, and `--accept` leaves both where they stand.
    for (name, fences) in [
        ("repin-pinned", "/// ```expect exit=0\n/// ```\n".to_owned()),
        ("repin-absent", "/// ```expect\n/// ```\n".to_owned()),
    ] {
        let text = format!("/// ```php\n/// exit(3);\n/// ```\n{fences}pub fn native() {{}}\n");
        let root = scratch(name, &text);
        let reading = run(&root, &registry(""), Accept::Status);
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert_eq!(count(&reading, "failed"), 1);
        assert_eq!(count(&reading, "accepted"), 0);
        assert_eq!(
            fs::read_to_string(root.join("ext/fixture/src/lib.rs")).unwrap(),
            text,
            "nothing is written"
        );
    }
}

/// `--accept --streams` writes the oracle's normalised streams into each
/// fence marked `capture`, the `expect` fence's body and a `stderr` fence's,
/// and leaves a fence not marked byte for byte; the blocks of one file are
/// written whatever their lengths become, and a second run writes nothing
/// (harness §8, #801).
#[test]
fn accept_streams_writes_the_marked_fences_alone() {
    if oracle().is_none() {
        return;
    }
    let unmarked =
        "/// ```php\n/// echo 2;\n/// ```\n/// ```expect\n/// %d\n/// ```\npub fn two() {}\n";
    let text = format!(
        "{}{unmarked}{}",
        "/// ```php\n/// ini_set('display_errors', 'stderr');\n/// echo \"one\\n\\ntwo\", $u;\n/// ```\n/// ```expect capture\n/// stale\n/// ```\npub fn one() {}\n",
        "/// ```php\n/// echo 3;\n/// ```\n/// ```expect capture\n/// x\n/// ```\npub fn three() {}\n",
    );
    let root = scratch("streams", &text);
    let file = root.join("ext/fixture/src/lib.rs");

    let reading = run(&root, &registry(""), Accept::Status);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "accepted"), 0);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        text,
        "`--accept` alone writes no stream"
    );

    let reading = run(&root, &registry(""), Accept::Streams);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "accepted"), 2, "{reading:#?}");
    let written = fs::read_to_string(&file).unwrap();
    assert!(
        written.starts_with(concat!(
            "/// ```php\n/// ini_set('display_errors', 'stderr');\n/// echo \"one\\n\\ntwo\", $u;\n/// ```\n",
            "/// ```expect capture\n/// one\n///\n/// two\n/// ```\n",
            "/// ```stderr\n/// Warning: Undefined variable $u in %SCRATCH%/witness.php on line 3\n/// ```\n",
            "pub fn one() {}\n",
        )),
        "{written}"
    );
    assert!(
        written.contains(unmarked),
        "the unmarked fence moved: {written}"
    );
    assert!(
        written.ends_with("/// ```expect capture\n/// 3\n/// ```\npub fn three() {}\n"),
        "{written}"
    );

    let again = run(&root, &registry(""), Accept::Streams);
    assert_eq!(again.status, Status::Pass, "{again:#?}");
    assert_eq!(count(&again, "accepted"), 0);
    assert_eq!(fs::read_to_string(&file).unwrap(), written);
}

/// The negative control of the capture (harness §4.1, §8; #801): the
/// fixture's one fence is marked `capture` and prints a length its scratch
/// directory moves, so the oracle's two runs differ and the capture
/// writes nothing and leaves the crate failing; the note naming the line
/// is held beside the judge. The run is bounded: one that waits on a pipe
/// something outside it holds fails in twenty seconds rather than holding a
/// hook for good, as it held one for 23 minutes on a pipe a `php-linked`
/// server had inherited (#2550).
#[test]
fn a_capture_whose_two_runs_differ_writes_nothing_and_fails_the_crate() {
    if oracle().is_none() {
        return;
    }
    let root = fixture_at("witness", "capture-moves");
    let file = root.join("ext/pcre/src/lib.rs");
    let before = fs::read_to_string(&file).unwrap();
    let manifest = Manifest::load(&root).unwrap();
    // A run past the bound fails the test and leaves its thread waiting;
    // the test binary's exit ends it.
    let (tell, ran) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ask = Ask {
            crates: vec!["ext/pcre".to_owned()],
            accept: Accept::Streams,
            ..Ask::default()
        };
        tell.send(witness::run(&root, &manifest, &Registry::default(), &ask))
    });
    let readings = ran
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the run ended within twenty seconds")
        .unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "unpinned"), 1, "{reading:#?}");
    assert_eq!(count(reading, "accepted"), 0, "{reading:#?}");
    assert!(reading.message.contains("lib.rs:15"), "{}", reading.message);
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        before,
        "nothing is written"
    );
}

/// The natives the attributes declare, read with the oracle away so the
/// coverage half is all that is judged (harness §4.5 condition 2,
/// revision 58): the subjects are the attribute-declared `fn`s and the
/// methods with a body, the plain `pub fn` beside them and the body-less
/// alias none. Coverage is what a fence's run through the linked engine
/// entered, and a fixture's natives are linked into no engine, so no fence
/// enters one and every subject reads `missing`, however its fences spell
/// the calls: a spelling covers nothing. The covered case needs a native
/// an engine links, held by `anchor`'s unit tests over a trace in the
/// expansion's own names and by `php_bundle`'s traced parity test.
#[test]
fn the_attributes_declare_the_natives_and_a_fence_covers_only_what_it_ran() {
    let coverage = |name: &str| {
        let root = fixture_at("witness", name);
        let mut manifest = Manifest::load(&root).unwrap();
        manifest.pin.oracle.path = root.join("no-php-here");
        let readings = witness::run(
            &root,
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["ext/pcre".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        readings.into_iter().next().unwrap()
    };

    let missing = coverage("anchor-missing");
    assert_eq!(missing.status, Status::Fail, "{missing:#?}");
    assert_eq!(count(&missing, "subjects"), 4, "{missing:#?}");
    assert_eq!(count(&missing, "missing"), 4, "{missing:#?}");
    for name in ["fn preg_last_error", "fn Pattern::replace"] {
        assert!(missing.message.contains(name), "{}", missing.message);
    }
    assert!(!missing.message.contains("helper"), "{}", missing.message);

    let spelled = coverage("anchor-covered");
    assert_eq!(spelled.status, Status::Fail, "{spelled:#?}");
    assert_eq!(count(&spelled, "subjects"), 3, "{spelled:#?}");
    assert_eq!(
        count(&spelled, "missing"),
        3,
        "a fence that spells a call to a native no engine links covers nothing: {spelled:#?}"
    );
}

/// An anchored crate is counted by its natives and by its kernels (#1264;
/// design §8): the fixture's engine declares one native on the attributes
/// and holds two kernels in `ops`, one without a witness. Read with the
/// oracle away, the native reads `missing` as any fixture's does (no engine
/// links it), the witnessed kernel is covered by the fence in its own doc
/// block, and the kernel without one is named. The plain `pub fn` outside `ops` and
/// the private function inside it are no subjects. The control: before
/// #1264 the same fixture counted one subject, the native, and named no
/// kernel.
#[test]
fn an_anchored_crate_counts_its_kernels_beside_its_natives() {
    let root = fixture_at("witness", "anchor-kernel");
    let mut manifest = Manifest::load(&root).unwrap();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["php_engine".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    let [reading] = readings.as_slice() else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "subjects"), 3, "{reading:#?}");
    assert_eq!(count(reading, "missing"), 2, "{reading:#?}");
    assert!(
        reading.message.contains("ops/mod.rs:15 pub fn sub"),
        "{}",
        reading.message
    );
    for absent in ["pub fn add", "helper"] {
        assert!(!reading.message.contains(absent), "{}", reading.message);
    }
}

/// The one reading of the `request fpm` fixture `name`, run with `host`
/// standing in for the pin's php-fpm.
fn served_by_a_stand_in(name: &str, host: PathBuf) -> Reading {
    let root = fixture_at("witness", name);
    let mut manifest = Manifest::load(&root).unwrap();
    let mut fpm = manifest.pin.sapi.fpm.clone().unwrap_or_default();
    fpm.path = host;
    manifest.pin.sapi.fpm = Some(fpm);
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    readings.into_iter().next().unwrap()
}

/// That `reading` is one failed run of the fixture's one fence, and no
/// native without a witness.
fn a_failed_run_and_no_missing_witness(reading: &Reading) {
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "failed"), 1, "{reading:#?}");
    assert_eq!(count(reading, "subjects"), 1, "{reading:#?}");
    assert_eq!(
        count(reading, "missing"),
        0,
        "a transport failure is a failed run, not a missing witness: {reading:#?}"
    );
    assert!(
        reading.message.contains("1 of 1 witnesses missed")
            && !reading.message.contains("without a witness"),
        "the fence's run failed, and no native is without one: {}",
        reading.message
    );
}

/// A `request fpm` fence whose oracle server resets the connection is a
/// failed run of the witness, its bork line printing the transport's
/// error, and never a native without a witness (#1426): the fixture's one
/// native carries such a fence, and the pin's php-fpm here is a stand-in
/// that accepts the request on the socket the configuration names and
/// drops it unanswered, as a reset leaves it. The control: a runner that
/// reads the fence as having entered nothing counts the native `missing`.
#[test]
fn a_request_fence_whose_server_resets_fails_the_run_and_is_not_a_missing_witness() {
    if oracle().is_none() {
        return;
    }
    let hosts = Tree::new("witness-reset-host").unwrap();
    let host = hosts.join("php-fpm");
    fs::write(
        &host,
        "#!/usr/bin/perl\nuse Socket;\nmy ($conf, $path);\nfor my $i (0 .. $#ARGV) { $conf = $ARGV[$i + 1] if $ARGV[$i] eq '-y'; }\nopen(my $f, '<', $conf) or die;\nwhile (<$f>) { $path = $1 if /^listen = (\\S+)/; }\nsocket(my $s, PF_UNIX, SOCK_STREAM, 0) or die;\nbind($s, sockaddr_un($path)) or die;\nlisten($s, 5) or die;\nwhile (accept(my $c, $s)) { my $b; sysread($c, $b, 8); close($c); }\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&host, fs::Permissions::from_mode(0o755)).unwrap();
    }
    a_failed_run_and_no_missing_witness(&served_by_a_stand_in("request-reset", host));
}

/// The same for the two ways a server can fail a fence once it has read
/// the whole request (#1493): the fixture `request-reset-sent`'s stand-in
/// ends the connection partway through the first record of its response,
/// and `request-empty`'s closes it with no response at all. Each fixture
/// carries its stand-in as `php-fpm` beside its manifest.
#[test]
fn a_request_fence_whose_server_fails_after_the_request_is_sent_is_a_failed_run() {
    if oracle().is_none() {
        return;
    }
    for name in ["request-reset-sent", "request-empty"] {
        let host = fixture_at("witness", name).join("php-fpm");
        a_failed_run_and_no_missing_witness(&served_by_a_stand_in(name, host));
    }
}

/// A native the build that links the engine compiled out is no subject
/// (#1068): the `#[cfg]` on the `fn`, on its impl, on its inline `mod`,
/// on the `mod name;` of its file or as the file's own `#![cfg]` is read
/// against the target this harness was built for. The fixture's natives
/// are linked into no engine, so every subject reads missing, and the
/// ones a `windows` cfg takes out are never named; the Darwin-only and the
/// Linux-only one are a subject each on its own host.
#[test]
fn a_native_under_a_cfg_the_target_denies_is_no_subject() {
    let root = fixture_at("witness", "anchor-cfg");
    let mut manifest = Manifest::load(&root).unwrap();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    let [reading] = readings.as_slice() else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    let mut named = vec!["fixture_everywhere", "fixture_unix", "fixture_feature"];
    let (here, elsewhere) = if cfg!(target_vendor = "apple") {
        ("fixture_apple", "fixture_linux")
    } else {
        ("fixture_linux", "fixture_apple")
    };
    named.push(here);
    assert_eq!(count(reading, "subjects"), 4, "{reading:#?}");
    assert_eq!(count(reading, "missing"), 4, "{reading:#?}");
    for name in named {
        assert!(reading.message.contains(name), "{}", reading.message);
    }
    for name in [
        elsewhere,
        "fixture_windows",
        "Pattern::replace",
        "fixture_inline",
        "fixture_platform",
        "fixture_elsewhere",
    ] {
        assert!(
            !reading.message.contains(name),
            "{name}: {}",
            reading.message
        );
    }
}

/// The negative control of the regime a fence may name (harness §4.1, §8,
/// ledger 55): a workspace whose one fence names a regime `oracle.toml`
/// does not carry, which the instrument must fail on. The name is read off
/// the pin before anything runs, so the same failure comes with the pinned
/// binary here and absent, and it names the fence, the regime asked for and
/// the regimes the pin has.
#[test]
fn a_fence_naming_a_regime_the_pin_does_not_carry_fails_the_crate_and_names_it() {
    let root = fixture_at("witness", "regime");
    let manifest = Manifest::load(&root).unwrap();
    for absent in [false, true] {
        let mut manifest = manifest.clone();
        if absent {
            manifest.pin.oracle.path = root.join("no-php-here");
        } else if oracle().is_none() {
            continue;
        }
        let readings = witness::run(
            &root,
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["ext/pcre".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        let reading = &readings[0];
        assert_eq!(
            reading.status,
            Status::Fail,
            "absent={absent}: {reading:#?}"
        );
        assert_eq!(count(reading, "unread"), 1, "{reading:#?}");
        assert_eq!(count(reading, "run"), 0, "{reading:#?}");
        assert!(
            reading.message.contains("ext/pcre/src/lib.rs:13")
                && reading.message.contains("no-such-regime")
                && reading.message.contains("short-tags"),
            "{}",
            reading.message
        );
    }
}

/// The negative control of the oracle's run (harness §4.1, §8; #783): a
/// workspace whose one fence pins what the linked engine prints and the
/// oracle does not, which the instrument must fail on, since what a fence
/// pins is the oracle's bytes.
#[test]
fn a_fence_the_oracle_does_not_meet_fails_the_crate() {
    if oracle().is_none() {
        return;
    }
    let root = fixture_at("witness", "oracle-unmet");
    let manifest = Manifest::load(&root).unwrap();
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "failed"), 1, "{reading:#?}");
    assert_eq!(count(reading, "passed"), 0, "{reading:#?}");
}

/// The negative control of a fence a `trace:` row covers (harness §4.1,
/// §4.3, §8; #1650): the fixture's two fences end the oracle on a signal,
/// and the crate fails on the one no row covers; the covered one is met
/// against the engine alone, counted `covered`, and the reading records
/// the oracle's signal for it.
#[test]
fn a_fence_a_trace_row_covers_records_the_oracle_and_the_one_it_does_not_fails_the_crate() {
    if oracle().is_none() {
        return;
    }
    let root = fixture_at("witness", "oracle-signal");
    let manifest = Manifest::load(&root).unwrap();
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::load(&root).unwrap(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "failed"), 1, "{reading:#?}");
    assert_eq!(count(reading, "covered"), 1, "{reading:#?}");
    assert!(
        reading.message.contains("ext/pcre/src/lib.rs:26")
            && reading.message.contains("1 fence held to the engine alone")
            && reading.message.contains("not judged: signal 9"),
        "{}",
        reading.message
    );
}

/// A witness that cannot run for want of the pinned binary is a snippet
/// that did not run, never a refusal: coverage is complete, and the reading
/// is a skip that counts what did not run (#65). Green would be a claim.
#[test]
fn a_crate_whose_witnesses_cannot_run_for_want_of_the_oracle_is_a_skip_never_a_pass() {
    let root = scratch("witness-absent", &block("/// ```expect\n/// 1\n/// ```\n"));
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "run"), 0);
    assert_eq!(count(reading, "not_run"), 1);
    assert_eq!(count(reading, "missing"), 0);
    assert!(
        reading.message.contains("coverage complete, oracle absent")
            && reading.message.contains("1 witness did not run")
            && reading.message.contains("no-php-here"),
        "{}",
        reading.message
    );
}

#[test]
fn the_reading_carries_run_failed_and_missing_and_is_scoped_to_the_crate() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("scoped", &block("/// ```expect\n/// 1\n/// ```\n"));
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.instrument, "witness");
    assert_eq!(reading.scope, "ext/fixture");
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert_eq!(count(&reading, "missing"), 0);
    assert!(!reading.key().is_empty(), "the reading is stamped");
}

/// A checkout with no binary on either target path is still a reading of
/// the engine: the linked engine serves the snippet in this process, the
/// other adapter of design §8's `ScriptExec` seam, so the reading is a
/// pass on its own merits and not the skip that said the engine ran none
/// (#119).
#[test]
fn with_no_binary_built_the_linked_engine_serves_the_witness() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("linked", &block("/// ```expect\n/// 1\n/// ```\n"));
    for profile in ["release-da", "release", "debug"] {
        let binary = root.join("target").join(profile).join("php");
        assert!(!binary.is_file(), "{} stands", binary.display());
    }
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
    assert!(
        !reading.message.contains("the engine ran none"),
        "{}",
        reading.message
    );
}

/// And a binary standing at `target/debug/php` is not what serves a
/// witness: the lane resolves none, so the reading is what the linked
/// engine wrote whether or not one is built (harness §7, revision 48;
/// #401). The stand-in writes bytes no fence pins and ends on 7, so a lane
/// that ran it would read a fail here.
#[test]
fn a_binary_in_the_tree_is_not_what_serves_a_witness() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("binary-stands", &block("/// ```expect\n/// 1\n/// ```\n"));
    a_binary_where_the_engine_s_would_be(&root);
    assert!(root.join("target/debug/php").is_file());
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 1);
    assert_eq!(count(&reading, "passed"), 1);
    assert_eq!(count(&reading, "failed"), 0);
}

/// And a fail is a fail there: what the linked engine wrote is held to
/// what the fences pin, with no binary standing anywhere (#119).
#[test]
fn with_no_binary_built_a_witness_the_linked_engine_misses_fails() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("linked-differs", &block("/// ```expect\n/// 2\n/// ```\n"));
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "failed"), 1);
    assert!(
        reading.message.contains("1 of 1 witnesses missed"),
        "{}",
        reading.message
    );
}

/// A crate the manifest marks `feature =` is held to its witnesses under
/// the linked runner as any crate is: the linked engine carries every
/// extension feature, so a fence that pins what the engine misses reads a
/// fail on its merits, no `unlinked` number and no skip naming the feature
/// (harness §7, revision 43; #314). This is the reading #292's skip stood
/// in place of.
#[test]
fn with_no_binary_built_a_crate_behind_a_feature_is_held_to_its_witnesses() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("linked-feature", &block("/// ```expect\n/// 2\n/// ```\n"));
    let mut manifest = fixture_manifest();
    manifest.crates[0] = manifest.crates[0].clone().with_feature("curl");
    let readings = witness::run(&root, &manifest, &registry(""), &Ask::default()).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(!reading.numbers.contains_key("unlinked"), "{reading:#?}");
    assert_eq!(count(reading, "run"), 1);
    assert_eq!(count(reading, "failed"), 1);
    assert!(
        !reading.message.contains("behind feature"),
        "{}",
        reading.message
    );
    assert!(!reading.key().is_empty(), "{reading:#?}");
}

/// And the four extensions outside php's default build are in the linked
/// engine: the one line each of their crates' witnesses asks,
/// `extension_loaded('<name>')`, reads `bool(true)` from the engine and
/// from the oracle, so the reading is a pass and not the fail #292 read.
#[test]
fn with_no_binary_built_the_linked_engine_carries_each_feature_extension() {
    if oracle().is_none() {
        return;
    }
    for name in ["mbstring", "pdo_mysql", "openssl", "curl"] {
        let root = scratch(
            &format!("linked-feature-{name}"),
            &format!(
                "/// ```php\n/// var_dump(extension_loaded('{name}'));\n/// ```\n/// ```expect\n/// bool(true)\n/// ```\npub fn native() {{}}\n"
            ),
        );
        let mut manifest = fixture_manifest();
        manifest.crates[0] = manifest.crates[0].clone().with_feature(name);
        let readings = witness::run(&root, &manifest, &registry(""), &Ask::default()).unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        let reading = &readings[0];
        assert_eq!(reading.status, Status::Pass, "{name}: {reading:#?}");
        assert_eq!(count(reading, "run"), 1, "{name}");
        assert_eq!(count(reading, "passed"), 1, "{name}");
        assert!(
            !reading.numbers.contains_key("unlinked"),
            "{name}: {reading:#?}"
        );
    }
}

/// What makes the two above true, held where it is written: the bundle's
/// `linked` feature is built of `default`, `one_thread` (`host` with pcntl
/// and posix) and the extensions no host links, and between them they name
/// every extension feature the
/// bundle declares, so the linked engine is the extensions the oracle has
/// and not the default set (harness §7, revision 43), `pcntl` among them
/// as the CLI carries it (revision 84); `php-linked`, the binary the lanes
/// serve a snippet through, is `sapi/cli` built with that set; and `xtask`
/// links no bundle of its own (revision 84; #2166). `new-ext` writes a new
/// crate into `default` and `host`, so an extension a branch adds is
/// linked at its merge (#1252, #3081). A feature the bundle gains that
/// none of the four names is this test red.
#[test]
fn php_linked_is_the_cli_with_every_extension_feature_and_xtask_links_none() {
    let bundle: toml::Value = toml::from_str(
        &fs::read_to_string(workspace().join("crates/php_bundle/Cargo.toml")).unwrap(),
    )
    .unwrap();
    let features = bundle["features"].as_table().unwrap();
    let declared: Vec<&str> = features
        .iter()
        .filter(|(_, on)| {
            on.as_array().is_some_and(|on| {
                on.iter()
                    .any(|dep| dep.as_str().is_some_and(|d| d.starts_with("dep:php_ext_")))
            })
        })
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(declared.contains(&"standard"), "{declared:?}");
    let named = |set: &str| -> Vec<&str> {
        features[set]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect()
    };
    let linked = named("linked");
    assert!(
        linked.contains(&"default") && linked.contains(&"one_thread"),
        "{linked:?}"
    );
    let one_thread = named("one_thread");
    assert!(one_thread.contains(&"host"), "{one_thread:?}");
    let reached: Vec<&str> = [named("default"), named("host"), one_thread, linked].concat();
    for feature in &declared {
        assert!(
            reached.contains(feature),
            "the bundle's `linked` does not reach {feature}: {reached:?}"
        );
    }
    let cli: toml::Value =
        toml::from_str(&fs::read_to_string(workspace().join("sapi/cli/Cargo.toml")).unwrap())
            .unwrap();
    let served: Vec<&str> = cli["features"]["linked"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert_eq!(served, ["php_bundle/linked", "php_bundle/test"]);
    let xtask: toml::Value =
        toml::from_str(&fs::read_to_string(workspace().join("xtask/Cargo.toml")).unwrap()).unwrap();
    assert!(
        xtask["dependencies"].get("php_bundle").is_none(),
        "the harness links no engine of its own"
    );
}

/// Nothing to run is complete only with the pinned oracle here to have
/// run it: green means the oracle agreed, and without the oracle the pass
/// would be a claim (#60). The engine's absence is another matter: the
/// coverage half was measured whatever stands in for it.
#[test]
fn a_crate_with_no_witness_and_nothing_that_needs_one_passes_with_the_oracle() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("nothing", "pub fn native() {}\n");
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(count(&reading, "run"), 0);
    assert!(
        reading.message.contains("no witness"),
        "{}",
        reading.message
    );
}

#[test]
fn a_crate_with_no_witness_and_the_oracle_absent_is_a_skip_that_says_so() {
    let root = scratch("nothing-absent", "pub fn native() {}\n");
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.path = root.join("no-php-here");
    let readings = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert_eq!(count(reading, "run"), 0);
    assert_eq!(count(reading, "missing"), 0);
    assert!(
        reading.message.contains("coverage complete, oracle absent"),
        "{}",
        reading.message
    );
    assert!(
        reading.message.contains("no-php-here"),
        "the skip names where the pin looked: {}",
        reading.message
    );
}

/// The skip is for a binary that is not here. One that is here and is not
/// the pin's refuses the run, nothing to run or not (§4.3).
#[test]
fn a_crate_with_no_witness_under_a_drifted_oracle_refuses_the_run() {
    if oracle().is_none() {
        return;
    }
    let root = scratch("nothing-drifted", "pub fn native() {}\n");
    let mut manifest = fixture_manifest();
    manifest.pin.oracle.md5 = "0".repeat(32);
    let err = witness::run(
        &root,
        &manifest,
        &registry(""),
        &Ask {
            crates: vec!["ext/fixture".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("the pin says"), "{err}");
}

#[test]
fn a_block_the_grammar_cannot_read_fails_the_crate_where_it_stands() {
    let root = scratch(
        "unread-block",
        "/// ```php\n/// echo 1;\n/// ```\n/// prose\npub fn native() {}\n",
    );
    let reading = run(&root, &registry(""), Accept::Nothing);
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(&reading, "unread"), 1);
    assert!(
        reading.message.contains("ext/fixture/src/lib.rs:1"),
        "{}",
        reading.message
    );
}

/// A crate whose sources cannot be read is a failing reading naming the
/// crate and the file, never a crate quietly skipped: `done` asks it for a
/// witness reading, and a pass over sources the grammar never saw would
/// say the crate was measured when it was not.
#[test]
fn a_crate_whose_sources_cannot_be_read_fails_and_names_the_crate() {
    let root = Tree::new("witness-unread").unwrap();
    fs::create_dir_all(root.join("ext/pcre/src")).unwrap();
    fs::write(
        root.join("ext/pcre/Cargo.toml"),
        "[package]\nname = \"php_ext_pcre\"\n",
    )
    .unwrap();
    // Two bytes no text is: the file is there and is not one the fence
    // grammar can read.
    fs::write(root.join("ext/pcre/src/lib.rs"), [0xff, 0xfe]).unwrap();
    Repo::init(&root).commit("scratch");
    let mut manifest = Manifest::named("ext/pcre");
    manifest.pin = pin();
    let readings = witness::run(
        &root,
        &manifest,
        &Registry::default(),
        &Ask {
            crates: vec!["ext/pcre".to_owned()],
            staged: false,
            ..Ask::default()
        },
    )
    .unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.starts_with("ext/pcre:") && reading.message.contains("src/lib.rs"),
        "{}",
        reading.message
    );
}

/// `--staged` is pre-commit's call (harness §6): the crates with a staged
/// change and no other, one reading each, and no reading when no crate is
/// touched, as `clippy --staged` answers.
#[test]
fn staged_narrows_the_run_to_the_crates_with_a_staged_change() {
    let root = scratch("staged", "pub fn native() {}\n");
    let staged = |crates: Vec<String>| {
        witness::run(
            &root,
            &fixture_manifest(),
            &Registry::default(),
            &Ask {
                crates,
                staged: true,
                ..Ask::default()
            },
        )
        .unwrap()
    };
    assert!(staged(Vec::new()).is_empty(), "nothing is staged yet");

    Repo::at(&root).git(&["add", "--", "ext/fixture"]);
    let readings = staged(Vec::new());
    assert_eq!(readings.len(), 1, "{readings:#?}");
    assert_eq!(readings[0].scope, "ext/fixture");
    assert_ne!(readings[0].status, Status::Fail, "{:#?}", readings[0]);
}

/// A witness that ends the process serving it is that witness's failure
/// and no other's (#1037, #1043, #2166): the fixture's first witness
/// sends `SIGABRT` to `php-linked`, which serves it. The witness borks
/// naming the signal, the crate's reading fails, and the witness after it
/// is served by a binary started again and judged; the harness itself
/// runs on. The fixture is laid over a scratch workspace, whose registry
/// and pin the run reads.
#[test]
fn a_witness_that_kills_its_server_fails_alone_and_the_next_is_served() {
    if oracle().is_none() {
        return;
    }
    let fixture = fixture_at("witness", "abort");
    let root = Tree::new("witness-abort").unwrap();
    scratch_workspace(workspace(), &root);
    Tracked::of(&fixture).with_pin("oracle.toml").write(&root);
    fs::create_dir_all(root.join("ext/pcre/src")).unwrap();
    for file in ["ext/pcre/Cargo.toml", "ext/pcre/src/lib.rs"] {
        fs::copy(fixture.join(file), root.join(file)).unwrap();
    }
    let ran = xtask(&root, &["witness", "--crate", "ext/pcre"]);
    assert_ne!(ran.code(), 0, "{ran}");
    let said = ran.stdout + &ran.stderr;
    let reading = Store::of_workspace(root.path())
        .read("witness", "ext/pcre")
        .unwrap()
        .unwrap();
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        said.contains("lib.rs:17") && said.contains("died on signal 6"),
        "the witness's line names the signal: {said}"
    );
    assert!(
        reading.message.contains("ext/pcre/src/lib.rs:17"),
        "the reading names the witness: {}",
        reading.message
    );
    assert_eq!(
        count(&reading, "passed"),
        1,
        "the witness after it is served and judged: {reading:#?}"
    );
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here builds the manifest it judges.
mod tracked {
    use super::*;

    /// The negative control of the manifest's coverage rule (harness §4.1, §12
    /// item 4): the fixture named by the rule's id must fail under it, with the
    /// rule's own text at the failure. Coverage is a count over the source and
    /// needs no php, so the same reading comes with the pinned binary absent
    /// (#65), where the fixture's one witness is a snippet that did not run.
    #[test]
    fn a_kernel_or_native_without_a_witness_is_missing_and_fails_the_crate() {
        let tracked = tracked_manifest();
        let tracked = tracked
            .rule_checked_by("witness-coverage")
            .expect("the manifest carries the coverage rule");
        let root = fixture(&tracked.id);
        let manifest = Manifest::load(&root).unwrap();
        let rule = manifest
            .rule_checked_by("witness-coverage")
            .expect("the fixture carries the rule");
        for absent in [false, true] {
            let mut manifest = manifest.clone();
            if absent {
                manifest.pin.oracle.path = root.join("no-php-here");
            } else if oracle().is_none() {
                continue;
            }
            let readings = witness::run(
                &root,
                &manifest,
                &Registry::default(),
                &Ask {
                    crates: vec!["ext/pcre".to_owned()],
                    staged: false,
                    ..Ask::default()
                },
            )
            .unwrap();
            assert_eq!(readings.len(), 1, "{readings:#?}");
            let reading = &readings[0];
            assert_eq!(
                reading.status,
                Status::Fail,
                "absent={absent}: {reading:#?}"
            );
            assert_eq!(reading.rule, tracked.id);
            assert_eq!(count(reading, "missing"), 1);
            assert_eq!(count(reading, "subjects"), 2);
            let ran = i64::from(!absent);
            assert_eq!(count(reading, "run"), ran, "{reading:#?}");
            assert_eq!(count(reading, "not_run"), 1 - ran, "{reading:#?}");
            assert!(
                reading.message.contains("ctype_alpha"),
                "{}",
                reading.message
            );
            assert!(reading.message.contains(&rule.text), "{}", reading.message);
            if absent {
                assert!(
                    reading.message.contains("1 witness did not run")
                        && reading.message.contains("no-php-here"),
                    "the failure says what did not run and where the pin looked: {}",
                    reading.message
                );
            }
        }
    }

    /// A kernel without a witness fails the crate whether or not the oracle
    /// is here: the coverage half is measured without it, and nothing asserts
    /// a pin whose binary is not at the path (#60, #65).
    #[test]
    fn a_kernel_without_a_witness_fails_whether_or_not_the_oracle_is_here() {
        let root = scratch("uncovered-absent", "pub fn native() {}\n");
        let mut manifest = fixture_manifest();
        manifest.rules = tracked_manifest().rules;
        manifest.pin.oracle.path = root.join("no-php-here");
        let rule = manifest
            .rule_checked_by("witness-coverage")
            .expect("the manifest carries the coverage rule")
            .clone();
        let readings = witness::run(
            &root,
            &manifest,
            &registry(""),
            &Ask {
                crates: vec!["ext/fixture".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap();
        let reading = &readings[0];
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert_eq!(count(reading, "missing"), 1);
        assert_eq!(count(reading, "run"), 0);
        assert!(
            reading.message.contains("native") && reading.message.contains(&rule.text),
            "{}",
            reading.message
        );
    }

    #[test]
    fn the_instrument_writes_one_reading_per_crate_and_names_an_unknown_one() {
        let manifest = tracked_manifest();
        let readings = witness::run(
            workspace(),
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["php_proto".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        assert_eq!(readings[0].scope, "php_proto");

        let unknown = witness::run(
            workspace(),
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["php_nope".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(unknown.contains("php_nope"), "{unknown}");
    }

    #[test]
    fn the_harness_crate_carries_no_witness_of_its_own() {
        let manifest = tracked_manifest();
        let readings = witness::run(
            workspace(),
            &manifest,
            &Registry::default(),
            &Ask {
                crates: vec!["xtask".to_owned()],
                staged: false,
                ..Ask::default()
            },
        )
        .unwrap();
        assert_eq!(readings[0].status, Status::Skip);
        assert!(
            readings[0].message.contains("carries no witness"),
            "{:#?}",
            readings[0]
        );
    }
}

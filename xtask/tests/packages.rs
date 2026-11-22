//! `xtask packages` at its command line, over the fixture list of
//! `xtask/fixtures/packages` (its README says what each package is): each
//! package laid down as a git repository at the commit the list pins, so
//! nothing is fetched from the network, its suite run under the pinned php
//! and under a stand-in for Lela's binary that runs the same php saying it
//! is Lela, and its development dependencies installed by a stand-in for
//! Composer. The readings are read back from the store. A proposal of the
//! list is held over the registry's answers recorded in `registry/`, laid
//! down beside the repositories, and steered over a scratch list.
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};

use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::pin;
use common::repo::scratch_workspace;
use common::run::{Ran, Run};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The fixture laid down in a tree of its own: its lists and lock as they
/// stand, and each package of `sources/` a git repository holding it at
/// one commit, made with a fixed author, committer and time, so the commit
/// is the one the lists pin on any machine.
fn laid() -> Tree {
    let tree = Tree::new("packages-fixture").unwrap();
    let from = xtask::package_dir().join("fixtures/packages");
    copied(&from, &tree);
    for entry in fs::read_dir(tree.join("sources")).unwrap() {
        let repo = entry.unwrap().path();
        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(["-c", "core.autocrlf=false", "-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(&repo)
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
                .env("GIT_AUTHOR_DATE", "2026-11-21T00:00:00Z")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
                .env("GIT_COMMITTER_DATE", "2026-11-21T00:00:00Z")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        let tree_id = git(&["write-tree"]);
        let commit = git(&["commit-tree", "--no-gpg-sign", &tree_id, "-m", "fixture"]);
        git(&["update-ref", "refs/heads/main", &commit]);
    }
    let registry = format!("file://{}", tree.join("registry").display());
    let sources = tree.join("sources").display().to_string();
    for dir in ["registry/explore", "registry/p2/fixture"] {
        for answer in fs::read_dir(tree.join(dir)).unwrap() {
            let answer = answer.unwrap().path();
            let text = fs::read_to_string(&answer).unwrap();
            let text = text.replace("@REGISTRY@", &registry);
            fs::write(&answer, text.replace("@SOURCES@", &sources)).unwrap();
        }
    }
    tree
}

/// `from`'s files copied under `to`, directories made as they are met.
fn copied(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            fs::create_dir_all(&target).unwrap();
            copied(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

/// The stand-ins in `bin/` under `tree`: Lela's binary, which runs the
/// pinned `php` saying it is Lela, and Composer, a php program that
/// installs nothing and is satisfied where a lock was laid in, and whose
/// update resolves the fixture's `empty.lock`.
fn stand_ins(tree: &Tree, php: &Path) -> PathBuf {
    let lela = format!(
        "#!/bin/sh\nLELA_FIXTURE_ENGINE=1 exec {} \"$@\"\n",
        php.display()
    );
    let bin = tree.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("lela"), lela).unwrap();
    xtask::scratch::executable(&bin.join("lela")).unwrap();
    let composer = format!(
        "<?php\nif (($argv[1] ?? '') === 'update') {{ exit(copy({:?}, 'composer.lock') ? 0 : 1); }}\n\
         exit(($argv[1] ?? '') === 'install' && is_file('composer.lock') ? 0 : 1);\n",
        tree.join("empty.lock").display().to_string()
    );
    fs::write(bin.join("composer"), composer).unwrap();
    bin.join("lela")
}

/// PATH with the stand-ins of `fixture` first.
fn on_path(fixture: &Tree) -> std::ffi::OsString {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fixture.join("bin")];
    paths.extend(std::env::split_paths(&path));
    std::env::join_paths(paths).unwrap()
}

/// A scratch workspace in a tree named `name`, carrying the services'
/// addresses a run gives every suite.
fn scratch(name: &str) -> Tree {
    let root = Tree::new(name).unwrap();
    scratch_workspace(workspace(), &root);
    fs::create_dir_all(root.join("ci/packages")).unwrap();
    let services = "ci/packages/services.env";
    fs::copy(workspace().join(services), root.join(services)).unwrap();
    root
}

/// `xtask packages --list <list>` over a scratch workspace, with the
/// stand-ins first on PATH: the run, and the workspace whose store it
/// wrote. None where the pinned php is not on this machine.
fn packages(list: &str) -> Option<(Ran, Tree)> {
    let php = pin().oracle.file().ok()?.to_path_buf();
    let root = scratch("packages-root");
    let fixture = laid();
    let lela = stand_ins(&fixture, &php);
    let ran = Run::at(&root)
        .args(["packages", "--list"])
        .args([fixture.join(list)])
        .args(["--engine"])
        .args([&lela])
        .env("PATH", on_path(&fixture))
        .go();
    drop(fixture);
    Some((ran, root))
}

/// The reading of `scope` the run wrote.
fn read(root: &Path, scope: &str) -> Reading {
    Store::of_workspace(root)
        .read("packages", scope)
        .unwrap()
        .unwrap_or_else(|| panic!("no packages reading of {scope}"))
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(xtask::reading::Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// The fixture that must fail: the list's run writes one reading per
/// package and one of the whole. A test the oracle fails, errors or skips
/// is outside and never met, whatever Lela does; one Lela fails or leaves
/// out of its log is missed, so its package fails and so does the whole. A
/// suite that does not run under the oracle, and a package the list marks
/// as needing a service the lane does not provide, are left out and named
/// in the whole's message, which is whole: every listed package was judged
/// or left out by rule.
#[test]
fn the_list_writes_one_reading_per_package_and_one_of_the_whole() {
    let Some((ran, root)) = packages("list.toml") else {
        return;
    };
    assert_eq!(ran.code(), 1, "a failing reading's exit: {ran}");

    let met = read(&root, "fixture/met");
    assert_eq!(met.status, Status::Pass, "{met:#?}");
    let counts = ["found", "met", "outside", "missed"].map(|k| count(&met, k));
    assert_eq!(counts, [1, 1, 2, 0], "{met:#?}");

    let missed = read(&root, "fixture/missed");
    assert_eq!(missed.status, Status::Fail, "{missed:#?}");
    let counts = ["found", "met", "outside", "missed", "unreported"].map(|k| count(&missed, k));
    assert_eq!(counts, [3, 1, 1, 2, 1], "{missed:#?}");
    assert!(
        missed.message.contains("Missed::testWorld") && missed.message.contains("Missed::testGone"),
        "{}",
        missed.message
    );

    for (scope, why) in [
        ("fixture/unrun", "its suite did not run under the oracle"),
        (
            "fixture/cache",
            "memcached, which the lane does not provide",
        ),
    ] {
        let left = read(&root, scope);
        assert_eq!(left.status, Status::Skip, "{left:#?}");
        assert!(left.message.contains(why), "{}", left.message);
    }

    let whole = read(&root, "packages");
    assert_eq!(whole.status, Status::Fail, "{whole:#?}");
    let counts = [
        "listed",
        "judged",
        "left_out",
        "every_met",
        "found",
        "met",
        "outside",
        "whole",
    ]
    .map(|k| count(&whole, k));
    assert_eq!(counts, [4, 2, 2, 1, 4, 2, 3, 1], "{whole:#?}");
    for named in ["fixture/unrun (", "fixture/cache ("] {
        assert!(whole.message.contains(named), "{}", whole.message);
    }
}

/// A source whose digest is not the list's is refused before anything of
/// it runs, and the package is not judged; the rest of the list is, the
/// whole's reading is a skip with `whole = 0` naming the package, and the
/// run exits as one that could not run to its end.
#[test]
fn a_source_whose_digest_differs_is_refused_and_the_run_exits_as_unfinished() {
    let Some((ran, root)) = packages("refused.toml") else {
        return;
    };
    assert_eq!(ran.code(), i32::from(xtask::exit::UNFINISHED), "{ran}");
    assert!(
        ran.stderr.contains("packages could not run to its end"),
        "{ran}"
    );
    let refused = read(&root, "fixture/met");
    assert_eq!(refused.status, Status::Skip, "{refused:#?}");
    assert!(
        refused.message.contains("refused: its source's digest is"),
        "{}",
        refused.message
    );
    assert_eq!(read(&root, "fixture/missed").status, Status::Fail);
    let whole = read(&root, "packages");
    assert_eq!(whole.status, Status::Skip, "{whole:#?}");
    assert_eq!(count(&whole, "whole"), 0, "{whole:#?}");
    assert!(
        whole
            .message
            .contains("not whole: 1 package not judged: fixture/met"),
        "{}",
        whole.message
    );
}

/// With no oracle on the machine nothing is fetched or run: each package's
/// reading is a skip naming where the pin looked, the whole's is a skip
/// with `whole = 0`, and the run exits as one that could not run to its
/// end. Nothing here needs php, git or the fixture's repositories.
#[test]
fn a_run_with_no_oracle_writes_its_skips_and_exits_as_unfinished() {
    let root = scratch("packages-no-oracle");
    let file = root.join("oracle.toml");
    let text = fs::read_to_string(&file).unwrap();
    let path = format!("path = \"{}\"", pin().oracle.path.display());
    let nowhere = format!("path = \"{}\"", root.join("nowhere/php").display());
    fs::write(&file, text.replacen(&path, &nowhere, 1)).unwrap();
    let list = xtask::package_dir().join("fixtures/packages/list.toml");
    let ran = Run::at(&root)
        .args(["packages", "--list"])
        .args([&list])
        .go();
    assert_eq!(ran.code(), i32::from(xtask::exit::UNFINISHED), "{ran}");
    for scope in [
        "fixture/met",
        "fixture/missed",
        "fixture/unrun",
        "fixture/cache",
    ] {
        let reading = read(&root, scope);
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains("nowhere/php"),
            "{}",
            reading.message
        );
    }
    let whole = read(&root, "packages");
    assert_eq!(
        (whole.status, count(&whole, "whole")),
        (Status::Skip, 0),
        "{whole:#?}"
    );
}

/// `xtask packages propose --out <out>` at `root` over the recorded
/// registry of `fixture`, with its stand-ins first on PATH.
fn proposal(root: &Path, fixture: &Tree, out: &Path) -> Ran {
    fs::create_dir_all(out).unwrap();
    Run::at(root)
        .args(["packages", "propose", "--out"])
        .args([out])
        .args(["--registry"])
        .args([format!("file://{}", fixture.join("registry").display())])
        .env("PATH", on_path(fixture))
        .go()
}

/// The table of the row naming `name` in a list's text.
fn row(text: &str, name: &str) -> toml::Table {
    let list: toml::Table = toml::from_str(text).unwrap();
    let rows = list["package"].as_array().unwrap();
    (rows.iter().map(|r| r.as_table().unwrap().clone()))
        .find(|r| r["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("no row of {name} in {text}"))
}

/// The proposal over the recorded registry, with no list standing: each
/// package of its two pages at its newest stable version, pinned by the
/// commit, source digest and lock digest `list.toml` pins it by, which a
/// run held to the source; its lock kept where its row names it; its
/// clients marking the services its suite needs; the day of the counts
/// named. The metapackage with no source and the source whose commit
/// cannot be fetched are named and left out. The instrument reads what
/// it wrote, holding a row's digests to the source it fetches.
#[test]
fn a_proposal_over_the_recorded_registry_pins_each_row_as_a_run_reads_it() {
    let Some(php) = pin().oracle.file().ok().map(Path::to_path_buf) else {
        return;
    };
    let root = scratch("packages-propose");
    let fixture = laid();
    let lela = stand_ins(&fixture, &php);
    let out = root.join("proposal");
    let ran = proposal(&root, &fixture, &out);
    assert_eq!(ran.code(), 0, "{ran}");
    for left in [
        "left out fixture/meta: the registry gives no git source commit",
        "left out fixture/gone: Git fetch -q --depth 1 ",
    ] {
        assert!(ran.stdout.contains(left), "{left}: {ran}");
    }
    let text = fs::read_to_string(out.join("packages.toml")).unwrap();
    let head = text.lines().next().unwrap();
    assert!(head.starts_with("# 2 rows proposed from file://"), "{text}");
    let day = head.split("install counts of ").nth(1).unwrap_or_default();
    assert!(
        day.len() > 10 && day.as_bytes()[4] == b'-' && day.as_bytes()[7] == b'-',
        "the day of the counts: {head}"
    );
    let listed = fs::read_to_string(fixture.join("list.toml")).unwrap();
    for name in ["fixture/met", "fixture/missed"] {
        let (proposed, pinned) = (row(&text, name), row(&listed, name));
        for key in ["version", "commit", "digest", "lock_digest"] {
            assert_eq!(proposed[key], pinned[key], "{name}'s {key}: {text}");
        }
        let lock = format!("packages/{name}.lock");
        assert_eq!(proposed["lock"].as_str(), Some(lock.as_str()));
        assert_eq!(
            fs::read(out.join(&lock)).unwrap(),
            fs::read(fixture.join("empty.lock")).unwrap()
        );
        assert!(!proposed.contains_key("runner"), "{text}");
    }
    assert!(!row(&text, "fixture/met").contains_key("needs"), "{text}");
    let needs = row(&text, "fixture/missed")["needs"].clone();
    assert_eq!(needs, toml::Value::from(vec!["memcached", "redis"]));
    assert_eq!(text.matches("[[package]]").count(), 2, "{text}");

    let ran = Run::at(&root)
        .args(["packages", "--list"])
        .args([out.join("packages.toml")])
        .args(["--package", "fixture/met", "--engine"])
        .args([&lela])
        .env("PATH", on_path(&fixture))
        .go();
    assert_eq!(ran.code(), 0, "{ran}");
    let met = read(&root, "fixture/met");
    assert!(
        met.message
            .contains("its suite did not run under the oracle")
            && !met.message.contains("refused"),
        "the row's digests held, and its runner is phpunit's: {}",
        met.message
    );
}

/// One package of the list, `--package`: its reading, and none of the
/// whole or of the list's other packages; a name the list does not carry
/// is refused before anything runs.
#[test]
fn one_package_of_the_list_writes_its_reading_and_none_of_the_whole() {
    let Some(php) = pin().oracle.file().ok().map(Path::to_path_buf) else {
        return;
    };
    let root = scratch("packages-one");
    let fixture = laid();
    let lela = stand_ins(&fixture, &php);
    let one = |name: &str| {
        Run::at(&root)
            .args(["packages", "--list"])
            .args([fixture.join("list.toml")])
            .args(["--package", name, "--engine"])
            .args([&lela])
            .env("PATH", on_path(&fixture))
            .go()
    };
    let ran = one("fixture/met");
    assert_eq!(ran.code(), 0, "{ran}");
    assert_eq!(read(&root, "fixture/met").status, Status::Pass);
    let store = Store::of_workspace(&root);
    for scope in ["packages", "fixture/missed"] {
        assert!(store.read("packages", scope).unwrap().is_none(), "{scope}");
    }
    let refused = one("fixture/nobody").failed();
    assert!(
        refused.contains("names no package fixture/nobody"),
        "{refused}"
    );
}

/// `steer` writes a proposal's rows over a scratch list once
/// `harness.self` names it: the proposal keeps the list's text whole and
/// adds the registry's package the list lacks after it, so steer reads
/// one row added and nothing changed, and the list is the proposal after.
#[test]
fn steer_writes_a_proposal_s_rows_over_a_scratch_list() {
    let Some(php) = pin().oracle.file().ok().map(Path::to_path_buf) else {
        return;
    };
    let root = Tree::new("packages-steer").unwrap();
    let repo = scratch_workspace(workspace(), &root);
    let fixture = laid();
    stand_ins(&fixture, &php);
    let listed = fs::read_to_string(fixture.join("list.toml")).unwrap();
    let missed = listed.split("\n[[package]]\n").nth(2).unwrap();
    let standing = format!("# A scratch list of one row.\n\n[[package]]\n{missed}");
    fs::create_dir_all(root.join("data")).unwrap();
    fs::write(root.join("data/packages.toml"), &standing).unwrap();
    let seams = fs::read_to_string(root.join("seams.toml")).unwrap();
    if !seams.contains("\"data/packages.toml\"") {
        let scope = "\"seams.toml\", \"oracle.toml\", \"docs/divergence.toml\"]";
        assert!(seams.contains(scope), "harness.self's scope");
        let widened = scope.replace(']', ", \"data/packages.toml\"]");
        repo.write("seams.toml", seams.replacen(scope, &widened, 1));
        repo.commit("harness.self names the list, as Mitchell's block widens it");
    }

    let out = root.join("proposal");
    assert_eq!(proposal(&root, &fixture, &out).code(), 0);
    let proposed = fs::read_to_string(out.join("packages.toml")).unwrap();
    assert!(proposed.starts_with(&standing), "{proposed}");
    assert!(
        proposed.contains("\n# 1 row proposed from file://"),
        "{proposed}"
    );
    row(&proposed, "fixture/met");

    let ran = Run::at(&root)
        .args(["steer", "data/packages.toml", "--from"])
        .args([out.join("packages.toml")])
        .go();
    assert_eq!(ran.code(), 0, "{ran}");
    let reading = Store::of_workspace(&root)
        .read("steer", "data/packages.toml")
        .unwrap()
        .expect("steer's reading");
    assert!(
        reading.message.ends_with(": package[fixture/met]"),
        "{}",
        reading.message
    );
    let now = fs::read_to_string(root.join("data/packages.toml")).unwrap();
    assert_eq!(now, proposed);
}

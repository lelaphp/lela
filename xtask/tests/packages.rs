//! `xtask packages` at its command line, over the fixture list of
//! `xtask/fixtures/packages` (its README says what each package is): each
//! package laid down as a git repository at the commit the list pins, so
//! nothing is fetched from the network, its suite run under the pinned php
//! and under a stand-in for Lela's binary that runs the same php saying it
//! is Lela, and its development dependencies installed by a stand-in for
//! Composer. The readings are read back from the store.
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
/// installs nothing and is satisfied where a lock was laid in.
fn stand_ins(tree: &Tree, php: &Path) -> PathBuf {
    let lela = format!(
        "#!/bin/sh\nLELA_FIXTURE_ENGINE=1 exec {} \"$@\"\n",
        php.display()
    );
    let bin = tree.join("bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("lela"), lela).unwrap();
    xtask::scratch::executable(&bin.join("lela")).unwrap();
    let composer =
        "<?php\nexit(($argv[1] ?? '') === 'install' && is_file('composer.lock') ? 0 : 1);\n";
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

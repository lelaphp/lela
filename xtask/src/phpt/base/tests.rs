//! The base's worktree goes however the call that laid it ends.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::Laid;
use crate::git::git;
use crate::scratch::{Tree, committing};

#[cfg(tracked)]
#[path = "../../../tests/common/repo.rs"]
mod repo;

/// A repository with one commit, and where a base's worktree is laid
/// beside it.
fn repository(name: &str) -> (Tree, Tree) {
    let (main, beside) = (
        Tree::new(name).unwrap(),
        Tree::new(&format!("{name}-beside")).unwrap(),
    );
    committing(&main, &["init", "-q", "-b", "main"]);
    std::fs::write(main.join("Cargo.toml"), "[workspace]\n").unwrap();
    committing(&main, &["add", "--", "Cargo.toml"]);
    committing(&main, &["commit", "-q", "-m", "base"]);
    (main, beside)
}

/// Whether git lists a worktree at `dir`.
fn listed(root: &Path, dir: &Path) -> bool {
    let list = git(root, &["worktree", "list", "--porcelain"]).unwrap();
    list.lines().any(|line| {
        line.strip_prefix("worktree ")
            .is_some_and(|at| Path::new(at).ends_with(dir.file_name().unwrap()))
    })
}

/// The process that owns a base's worktree stopped by a signal, which no
/// drop outlives, leaves no worktree behind: the watcher removes it once
/// the owner is gone, and git lists it no more.
#[test]
fn a_base_run_stopped_by_a_signal_leaves_no_worktree_behind() {
    let (main, beside) = repository("base-signalled");
    let dir = beside.join("base-0000000-1");
    let mut owner = Command::new("sleep").arg("60").spawn().unwrap();
    let laid = Laid::at(&main, &dir, "HEAD", owner.id()).unwrap();
    assert!(dir.is_dir() && listed(&main, &dir));
    let pid = nix::unistd::Pid::from_raw(i32::try_from(owner.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    owner.wait().unwrap();
    let waited = Instant::now();
    while dir.exists() && waited.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!dir.exists(), "the worktree outlived its owner");
    assert!(!listed(&main, &dir), "git still lists the worktree");
    drop(laid);
}

/// A call that returns early or panics drops its worktree, and one that
/// runs to the end removes it.
#[test]
fn a_base_run_that_returns_early_drops_its_worktree() {
    let (main, beside) = repository("base-dropped");
    let (dropped, removed) = (beside.join("base-0000000-2"), beside.join("base-0000000-3"));
    let laid = Laid::at(&main, &dropped, "HEAD", std::process::id()).unwrap();
    drop(laid);
    assert!(!dropped.exists() && !listed(&main, &dropped));
    let laid = Laid::at(&main, &removed, "HEAD", std::process::id()).unwrap();
    laid.remove().unwrap();
    assert!(!removed.exists() && !listed(&main, &removed));
}

#[test]
fn per_commit_is_an_explicit_base_selection_and_never_the_nightly_lane() {
    use crate::cli::{Cli, Command as Instrument};
    use clap::Parser as _;

    for args in [
        vec!["xtask", "phpt", "--base", "--per-commit"],
        vec!["xtask", "phpt", "--per-commit", "--base", "origin/main"],
        vec!["xtask", "phpt", "--base"],
        vec!["xtask", "phpt", "--base", "--subset", "Zend/tests"],
    ] {
        let parsed = Cli::try_parse_from(&args).unwrap();
        let Instrument::Phpt(parsed) = parsed.command else {
            panic!("not phpt")
        };
        assert_eq!(parsed.baseline.per_commit, args.contains(&"--per-commit"));
        assert!(!parsed.full);
    }
    for suffix in [
        vec![],
        vec!["--subset", "Zend/tests"],
        vec!["--full"],
        vec!["--jit", "force"],
        vec!["--engine", "php"],
        vec!["--control"],
        vec!["--against", "HEAD"],
        vec!["--unkept"],
        vec!["--subset", "Zend/tests", "--upstream", "one.phpt"],
    ] {
        let mut args = vec!["xtask", "phpt", "--per-commit"];
        if !suffix.is_empty() {
            args.push("--base");
        }
        args.extend(suffix);
        assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
    }
}

/// Selection and kept-run freshness under the calling checkout's recipe,
/// without building a base engine or running the corpus.
#[cfg(tracked)]
#[test]
fn all_per_commit_includes_untouched_subsets_and_reuses_only_fresh_runs() {
    use crate::manifest::{Crate, Manifest};
    use crate::oracle::Runner;
    use crate::phpt::{Ask, Judged, Judgement, Scope, against};
    use crate::reading::Reading;
    use crate::tally::Tally;
    use repo::Repo;

    let (root, _beside) = repository("base-selection");
    let repo = Repo::at(&root);
    repo.write(".gitignore", "target/\nbase/\n");
    for name in ["ctype", "pcre", "curl", "zend_test"] {
        repo.write(&format!("ext/{name}/src/lib.rs"), "// base\n");
        repo.write(
            &format!("corpus/ext/{name}/tests/one.phpt"),
            "--TEST--\none\n",
        );
    }
    repo.commit("subsets");
    let base = crate::done::Base::of(&root, Some("HEAD")).unwrap();
    repo.write("ext/ctype/src/lib.rs", "// touched\n")
        .commit("touched");
    let mut manifest = Manifest::of([
        Crate::named("ext/ctype"),
        Crate::named("ext/pcre"),
        Crate::named("ext/lexbor"),
        Crate::named("ext/curl").with_feature("curl"),
        Crate::named("ext/zend_test")
            .with_feature("zend_test")
            .with_oracle("zend-test"),
    ]);
    manifest.pin = Manifest::load(&crate::built_root())
        .unwrap()
        .pin
        .map(|pin| pin.on_this_machine(Some(root.join("corpus"))));
    let selected = |ask: &Ask, all| super::wanted(&root, &manifest, ask, &base, all).unwrap();
    assert_eq!(selected(&Ask::default(), false), ["ext/ctype"]);
    assert_eq!(
        selected(&Ask::default(), true),
        ["ext/ctype", "ext/pcre", "ext/zend_test"]
    );
    let single = Ask {
        subset: Some("pcre".to_owned()),
        ..Ask::default()
    };
    assert_eq!(selected(&single, false), ["ext/pcre"]);

    // Keep pcre at the base with the current recipe; the engine tree at
    // HEAD differs, so write it in an instrument-managed base worktree.
    let dir = root.join("base");
    let laid = Laid::at(&root, &dir, &base.commit, std::process::id()).unwrap();
    let runs = against::Runs::new(&dir, &manifest, None).unwrap();
    let tally = Tally {
        found: 1,
        judged: vec![Judged::new(
            "phpt:php-src/ext/pcre/tests/one.phpt",
            "one.phpt",
            Judgement::Pass,
            "",
        )],
    };
    runs.after(
        &manifest,
        &Scope::of("ext/pcre"),
        (&tally, &Runner::Engine(dir.join("engine/php"))),
        &Reading::new("phpt", "ext/pcre"),
        None,
    )
    .unwrap();
    against::carry(&dir, &root).unwrap();
    laid.remove().unwrap();
    assert_eq!(
        selected(&Ask::default(), true),
        ["ext/ctype", "ext/zend_test"]
    );
    assert!(selected(&single, false).is_empty());
    let mut recipe: toml::Value =
        toml::from_str(&std::fs::read_to_string(crate::built_root().join("oracle.toml")).unwrap())
            .unwrap();
    recipe["recipe"]["limits"]["timeout_s"] = toml::Value::Integer(
        recipe["recipe"]["limits"]["timeout_s"]
            .as_integer()
            .unwrap()
            + 1,
    );
    manifest.pin = Some(
        crate::oracle::Pin::parse(&toml::to_string(&recipe).unwrap())
            .unwrap()
            .on_this_machine(Some(root.join("corpus"))),
    );
    assert_eq!(
        super::wanted(&root, &manifest, &single, &base, false).unwrap(),
        ["ext/pcre"]
    );
}

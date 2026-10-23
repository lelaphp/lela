//! The comparison from inside: two kept runs diffed and the reading they
//! come to, over the fixture at `xtask/fixtures/phpt/against/`, which
//! plants one regression. Nothing here spawns php, so the negative control
//! runs on every machine; the run kept and found by a commit is held from
//! outside, in `tests/phpt.rs`.

use std::fs;

use super::*;
use crate::scratch::{Tree, committing};

fn fixture(name: &str) -> Run {
    let file = crate::package_dir()
        .join("fixtures/phpt/against")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap()
}

fn reading_of(diff: Result<Diff, String>) -> Reading {
    compared(
        Reading::new(INSTRUMENT, "ext/fixture@against"),
        "882bbb2",
        diff,
    )
    .unwrap()
}

/// A tally of one test of `ext/ctype`, met: a run the engine keeps.
fn one_met() -> Tally {
    let id = "phpt:php-src/ext/ctype/tests/one.phpt";
    let judged = crate::phpt::Judged::new(id, "one.phpt", Judgement::Pass, "");
    Tally {
        judged: vec![judged],
        found: 1,
    }
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(crate::reading::Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

#[test]
fn the_planted_regression_fails_the_reading_and_is_named() {
    let diff = diff(&fixture("base.json"), &fixture("head.json"), None).unwrap();
    assert_eq!(
        diff.regressed,
        [(
            "phpt:php-src/ext/fixture/tests/planted.phpt".to_owned(),
            Judgement::Fail
        )]
    );
    // kept, planted and diverged were met at the base; the skip met
    // nothing there, so its fail here is no regression.
    assert_eq!(diff.held, 3);
    assert_eq!(diff.fixed, 1);
    let reading = reading_of(Ok(diff));
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("1 fail here: ext/fixture/tests/planted.phpt"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "regressed"), 1);
    assert_eq!(count(&reading, "held"), 3);
    assert_eq!(count(&reading, "fixed"), 1);
}

#[test]
fn the_same_run_against_itself_passes() {
    let base = fixture("base.json");
    let reading = reading_of(diff(&base, &base, None));
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert!(
        reading.message.contains("none fails here"),
        "{}",
        reading.message
    );
    assert_eq!(count(&reading, "regressed"), 0);
}

/// Neither a test the compiler no longer reads nor one that skips here
/// meets a test met at the base (#725).
#[test]
fn a_test_the_compiler_no_longer_reads_or_that_skips_is_a_regression() {
    let base = fixture("base.json");
    for now in [Judgement::Unread, Judgement::Skip] {
        let mut head = base.clone();
        head.verdicts
            .insert("phpt:php-src/ext/fixture/tests/kept.phpt".to_owned(), now);
        let diff = diff(&base, &head, None).unwrap();
        assert_eq!(diff.regressed.len(), 1, "{diff:?}");
        assert_eq!(diff.regressed[0].1, now);
    }
}

/// Every way two runs fail to compare is a skip that says why, and never
/// the pass "no regressions" would be.
#[test]
fn runs_that_do_not_compare_give_no_verdict() {
    let base = fixture("base.json");

    let mut other_corpus = fixture("head.json");
    other_corpus.corpus = "php-8.5.10 0123abcd".to_owned();
    let mut fewer = fixture("head.json");
    fewer
        .verdicts
        .remove("phpt:php-src/ext/fixture/tests/planted.phpt");
    let mut none_met = base.clone();
    for verdict in none_met.verdicts.values_mut() {
        *verdict = Judgement::Fail;
    }

    for (head, why) in [
        (&other_corpus, "over the corpus"),
        (&fewer, "not the same tests"),
        (&none_met, "nothing is held"),
    ] {
        let was = if why == "nothing is held" {
            head
        } else {
            &base
        };
        let reading = reading_of(diff(was, head, None));
        assert_eq!(reading.status, Status::Skip, "{reading:#?}");
        assert!(
            reading.message.contains("no verdict"),
            "{}",
            reading.message
        );
        assert!(reading.message.contains(why), "{}", reading.message);
        assert!(reading.numbers.is_empty(), "{reading:#?}");
    }
}

#[test]
fn the_exit_is_the_comparisons_whatever_the_subsets_say() {
    let mut subset = Reading::new(INSTRUMENT, "ext/fixture");
    subset.status = Status::Fail;
    let mut against = Reading::new(INSTRUMENT, "ext/fixture@against");
    against.status = Status::Pass;
    assert!(answer(&[subset.clone(), against.clone()]).is_ok());
    for status in [Status::Fail, Status::Skip] {
        against.status = status;
        let err = answer(&[subset.clone(), against.clone()]).unwrap_err();
        assert!(err.to_string().contains("ext/fixture@against"), "{err:#}");
    }
}

#[test]
fn a_kept_run_is_spelled_as_the_fixture_is() {
    let base = fixture("base.json");
    let text = serde_json::to_string_pretty(&base).unwrap();
    let file = crate::package_dir().join("fixtures/phpt/against/base.json");
    assert_eq!(text.trim(), fs::read_to_string(file).unwrap().trim());
}

/// A run kept in a linked worktree at the base, where the skip asks for one
/// to be taken, is found by the commit from main's checkout (#1869). Before
/// it is kept, the skip names each worktree it looked in once, the one it
/// runs in first; from the linked one, main comes after it.
#[test]
fn a_run_kept_in_a_linked_worktree_at_the_base_is_found_from_main() {
    let main = Tree::new("against-main").unwrap();
    let beside = Tree::new("against-beside").unwrap();
    committing(&main, &["init", "-q", "-b", "main"]);
    fs::write(main.join("Cargo.toml"), "[workspace]\n").unwrap();
    committing(&main, &["add", "--", "Cargo.toml"]);
    committing(&main, &["commit", "-q", "-m", "base"]);
    let linked = beside.join("base");
    let linked_str = linked.to_str().unwrap();
    committing(
        &main,
        &["worktree", "add", "-q", "--detach", linked_str, "HEAD"],
    );

    let base = Base::at(&main, "HEAD").unwrap();
    let looked = roots(&main);
    assert_eq!(looked, [main.path(), &linked], "{looked:?}");
    assert_eq!(roots(&linked), [&linked, main.path()]);

    let why = base.run(&looked, "ext/fixture", "").unwrap().unwrap_err();
    let main_named = format!("{}, ", main.display());
    assert_eq!(why.matches(&main_named).count(), 1, "{why}");
    assert_eq!(why.matches(linked_str).count(), 1, "{why}");
    assert!(
        why.contains(&format!(
            "`cargo xtask phpt --base {} --subset ext/fixture`",
            base.commit
        )),
        "{why}"
    );

    let run = fixture("base.json");
    let unlisted = base
        .snapshot
        .tracked_over(&BuiltFrom::Unlisted.keyed(&main));
    let file = path(&linked, &unlisted.unwrap(), "ext/fixture");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, serde_json::to_string_pretty(&run).unwrap()).unwrap();
    let recipe = run.recipe.clone();
    assert_eq!(
        base.run(&roots(&main), "ext/fixture", &recipe).unwrap(),
        Ok(run)
    );
    // Under another pin the run is stale, and the base's call takes it
    // again (#2725).
    let stale = base.run(&looked, "ext/fixture", "moved").unwrap();
    assert_eq!(
        stale,
        Err(format!(
            "the run of ext/fixture kept at {0} is stale; oracle.toml changed since it was taken; take one with `cargo xtask phpt --base {0} --subset ext/fixture`",
            base.commit
        ))
    );

    // Another spelling of the checkout is the same root, and a worktree
    // git still lists whose directory is gone is none.
    assert_eq!(roots(&main.join(".")), looked);
    fs::remove_dir_all(&linked).unwrap();
    assert_eq!(roots(&main), [main.path()]);
}

/// A carried run takes the place of one standing under another pin that
/// was kept before it, and of no other (#2725): a base's run `phpt --base`
/// takes again under a moved pin is found fresh in main afterwards.
#[test]
fn a_carried_run_replaces_an_older_one_under_another_pin_and_no_other() {
    let (from, into) = (
        Tree::new("carry-from").unwrap(),
        Tree::new("carry-into").unwrap(),
    );
    let keep = |root: &Path, scope: &str, recipe: &str| {
        let run = Run {
            recipe: recipe.to_owned(),
            ..fixture("base.json")
        };
        let file = path(root, "tree", scope);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, serde_json::to_string(&run).unwrap()).unwrap();
    };
    let recipe =
        |root: &Path, scope: &str| read(&path(root, "tree", scope)).unwrap().unwrap().recipe;
    keep(&into, "ext/stale", "old");
    keep(&into, "ext/same", "new");
    std::thread::sleep(std::time::Duration::from_millis(20));
    keep(&from, "ext/stale", "new");
    keep(&from, "ext/same", "new");
    keep(&from, "ext/newer", "old");
    keep(&into, "ext/newer", "new");
    let carried = carry(&from, &into).unwrap();
    assert_eq!(
        carried,
        Carried {
            carried: 1,
            kept: 2
        }
    );
    assert_eq!(recipe(&into, "ext/stale"), "new");
    assert_eq!(
        recipe(&into, "ext/newer"),
        "new",
        "an older run never replaces a newer"
    );
}

/// After each subset, a run whose engine sources nobody writes asks git
/// nothing, however many subsets it runs (#2972); a source written while
/// a subset ran is still the comparison's skip that says so, and a file
/// touched and not written is asked of git once and then stands.
#[test]
fn after_asks_git_nothing_while_the_engines_sources_stand_still() {
    let tree = crate::key::fixture("against-after");
    let root: &Path = &tree;
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let runs = Runs::new(root, &manifest, Some("HEAD")).unwrap();
    let base = runs.base.as_ref().unwrap();
    let unlisted = base.snapshot.tracked_over(&BuiltFrom::Unlisted.keyed(root));
    let file = path(root, &unlisted.unwrap(), "ext/ctype");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let was = Run {
        commit: base.commit.clone(),
        corpus: runs.corpus.clone(),
        engine: "the linked engine".to_owned(),
        recipe: runs.recipe.clone(),
        verdicts: (one_met().judged.iter())
            .map(|j| (j.id.clone(), j.judgement))
            .collect(),
        misses: BTreeMap::new(),
        skips: BTreeMap::new(),
    };
    fs::write(&file, serde_json::to_string(&was).unwrap()).unwrap();
    let (held_as, subset) = (
        Scope::of("ext/ctype"),
        Reading::new(INSTRUMENT, "ext/ctype"),
    );
    let after = || {
        let held = runs.after(
            &manifest,
            &held_as,
            (&one_met(), &Runner::Linked),
            &subset,
            None,
        );
        held.unwrap().unwrap().message
    };
    let written = "the engine's sources were written while the subset ran";

    let (said, asked) = git::asked(root, || (0..5).map(|_| after()).collect::<Vec<_>>());
    assert_eq!(asked, Vec::<Vec<String>>::new());
    assert!(said.iter().all(|m| !m.contains(written)), "{said:?}");

    // Outside the engine's sources, a file written is no question.
    fs::write(root.join("notes.txt"), "beside the engine\n").unwrap();
    let (said, asked) = git::asked(root, after);
    assert!(
        asked.is_empty() && !said.contains(written),
        "{asked:?} {said}"
    );

    // Touched with the same bytes: asked of git once, and then it stands.
    let source = root.join("crates/php_engine/src/lib.rs");
    let bytes = fs::read(&source).unwrap();
    fs::write(&source, &bytes).unwrap();
    let (said, asked) = git::asked(root, || (after(), after()));
    assert!(!asked.is_empty() && !said.0.contains(written), "{said:?}");
    assert!(!said.1.contains(written), "{said:?}");
    let (_, again) = git::asked(root, after);
    assert!(again.is_empty(), "{again:?}");

    fs::write(&source, "// written while the subset ran\n").unwrap();
    assert!(after().contains(written));
    assert!(after().contains(written), "a source written stays written");
    fs::write(&source, &bytes).unwrap();
    assert!(
        !after().contains(written),
        "a source written back is the tree"
    );

    // A new source is the tree's once the index tracks it, and the index
    // is what moved.
    fs::write(root.join("crates/php_engine/src/new.rs"), "// new\n").unwrap();
    assert!(!after().contains(written), "an untracked file is no source");
    committing(root, &["add", "--", "crates/php_engine/src/new.rs"]);
    assert!(after().contains(written));
}

/// A git workspace whose `php_engine` and `sapi/cli` the binary at
/// `target/release-da/php` is built from, as the dep-info beside it says,
/// and whose `php_jit`, generated clippy configuration, guide and manifest
/// it is not; committed once.
fn workspace_with_a_binary(name: &str) -> (Tree, PathBuf) {
    let tree = Tree::new(name).unwrap();
    committing(&tree, &["init", "-q", "-b", "main"]);
    let files = [
        (".gitignore", "target/\n"),
        ("Cargo.toml", "[workspace]\n"),
        ("crates/php_engine/Cargo.toml", "[package]\n"),
        ("crates/php_engine/src/lib.rs", "// the kernels\n"),
        ("crates/php_jit/src/lib.rs", "// the compiled tier\n"),
        ("sapi/cli/src/main.rs", "fn main() {}\n"),
        ("ext/clippy.toml", "# generated\n"),
        ("ext/AGENTS.md", "# extensions\n"),
        ("seams.toml", "# the manifest\n"),
    ];
    for (file, text) in files {
        fs::create_dir_all(tree.join(file).parent().unwrap()).unwrap();
        fs::write(tree.join(file), text).unwrap();
    }
    committing(&tree, &["add", "-A"]);
    committing(&tree, &["commit", "-q", "-m", "a"]);
    let binary = tree.join("target/release-da/php");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "the engine").unwrap();
    let deps = ["crates/php_engine/src/lib.rs", "sapi/cli/src/main.rs"];
    let deps: Vec<String> = deps
        .iter()
        .map(|d| tree.join(d).display().to_string())
        .collect();
    let info = format!("{}: {}\n", binary.display(), deps.join(" "));
    fs::write(format!("{}.d", binary.display()), info).unwrap();
    (tree, binary)
}

/// `files` written with new text at `root`, and committed.
fn commit_writing(root: &Path, files: &[&str]) {
    for file in files {
        let path = root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("// {file}, written again\n")).unwrap();
    }
    committing(root, &["add", "-A"]);
    committing(root, &["commit", "-q", "-m", "written"]);
}

/// A run is kept under what its binary is built from (#3109): the
/// commits #3082 and #3103 made on 2026-10-20, `seams.toml` text,
/// `ext/clippy.toml` and a guide, with the compiled tier's crate the
/// default binary does not link, as `c5c2fb025` wrote, leave the run
/// standing, so `done` finds it as this tree's and, once that commit is
/// `origin/main`, as the next branch's base without a `--base` run
/// (#3110); a commit that writes a source the binary is compiled from
/// moves the key, and the kept run is refused, here and as the base.
#[test]
fn a_commit_that_leaves_the_binary_keeps_its_runs_and_one_that_moves_it_does_not() {
    let (tree, binary) = workspace_with_a_binary("against-built-from");
    let root: &Path = &tree;
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let pin = manifest.pin().unwrap();
    let (scope, engine) = ("ext/ctype", Runner::Engine(binary));
    let runs = Runs::new(root, &manifest, None).unwrap();
    let subset = Reading::new(INSTRUMENT, scope);
    let held_as = Scope::of(scope);
    let kept = runs.after(&manifest, &held_as, (&one_met(), &engine), &subset, None);
    assert_eq!(kept.unwrap(), None);
    assert!(kept_at(root, pin, "HEAD", scope).unwrap().is_ok());

    commit_writing(
        root,
        &[
            "seams.toml",
            "ext/clippy.toml",
            "ext/AGENTS.md",
            "harness-design.md",
            "crates/php_jit/src/lib.rs",
        ],
    );
    let now = Snapshot::disk(root).unwrap();
    assert!(
        !kept_run(&now, scope).unwrap().exists(),
        "the old key moved"
    );
    assert!(kept_at(root, pin, "HEAD", scope).unwrap().is_ok());
    assert_eq!(regressed(&now, "HEAD", pin, scope).unwrap(), Against::Held);
    // Carried into another checkout of the repository, as `worktree retire`
    // carries a branch's runs into main's, the run and the set it was kept
    // under are found there by the commit alone.
    let main = Tree::new("against-built-from-main").unwrap();
    let at = main.join("checkout");
    committing(
        root,
        &["clone", "-q", root.to_str().unwrap(), at.to_str().unwrap()],
    );
    assert!(
        kept_at(&at, pin, "HEAD", scope).unwrap().is_err(),
        "nothing kept there yet"
    );
    carry(root, &at).unwrap();
    assert!(kept_at(&at, pin, "HEAD", scope).unwrap().is_ok());

    commit_writing(root, &["crates/php_engine/src/lib.rs"]);
    let now = Snapshot::disk(root).unwrap();
    let why = kept_at(root, pin, "HEAD", scope).unwrap().unwrap_err();
    assert!(why.contains("no run of ext/ctype is kept at "), "{why}");
    let unrun = regressed(&now, "HEAD", pin, scope).unwrap();
    assert!(
        matches!(&unrun, Against::Unrun(why) if why.contains("no run of ext/ctype")),
        "{unrun:?}"
    );
}

/// A run of no test is no evidence about the tree (#3143): the engine's is
/// never kept, and one a store kept before, as main's kept a scratch
/// corpus's `ext/ctype` at 37474a3, is read as no run, at the base and
/// here. So `phpt --base` takes the base's run where only that one stands,
/// and the run it carries in replaces it, after which `done` has a verdict.
#[test]
fn a_run_of_no_test_is_no_run_and_the_bases_run_carried_in_replaces_it() {
    let (tree, binary) = workspace_with_a_binary("against-none");
    let root: &Path = &tree;
    let manifest = Manifest::load(&crate::built_root()).unwrap();
    let pin = manifest.pin().unwrap();
    let (scope, engine) = (Scope::of("ext/ctype"), Runner::Engine(binary));
    let runs = Runs::new(root, &manifest, None).unwrap();
    let after = |tally: &Tally| {
        let subset = Reading::new(INSTRUMENT, "ext/ctype");
        let kept = runs.after(&manifest, &scope, (tally, &engine), &subset, None);
        assert_eq!(kept.unwrap(), None);
    };
    let unrun = |root: &Path| {
        let why = kept_at(root, pin, "HEAD", "ext/ctype")
            .unwrap()
            .unwrap_err();
        assert!(why.contains("no run of ext/ctype is kept at "), "{why}");
        let now = Snapshot::disk(root).unwrap();
        let here = regressed(&now, "HEAD", pin, "ext/ctype").unwrap();
        assert!(matches!(here, Against::Unrun(_)), "{here:?}");
    };
    let none = Tally::default();
    assert_eq!(runs.run_kept_at(&scope, (&none, &engine)), None);
    after(&none);
    unrun(root);

    // The run of one test kept, and emptied as the store's was: no run.
    let at = root.join(runs.run_kept_at(&scope, (&one_met(), &engine)).unwrap());
    after(&one_met());
    let run = read(&at).unwrap().unwrap();
    let emptied = Run {
        verdicts: BTreeMap::new(),
        ..run.clone()
    };
    keep(&at, &emptied).unwrap();
    unrun(root);

    // `--base` takes it in a worktree of its own and carries it in.
    let base = Tree::new("against-none-base").unwrap();
    let there = base.join(at.strip_prefix(root).unwrap());
    fs::create_dir_all(there.parent().unwrap()).unwrap();
    keep(&there, &run).unwrap();
    assert_eq!(carry(&base, root).unwrap().carried, 1);
    assert_eq!(
        kept_at(root, pin, "HEAD", "ext/ctype").unwrap(),
        Ok(run.clone())
    );
    // A run of none carried after it, under another pin, replaces nothing.
    let later = Tree::new("against-none-later").unwrap();
    let there = later.join(at.strip_prefix(root).unwrap());
    fs::create_dir_all(there.parent().unwrap()).unwrap();
    let moved = Run {
        recipe: "moved".to_owned(),
        ..emptied
    };
    keep(&there, &moved).unwrap();
    assert_eq!(carry(&later, root).unwrap().carried, 0);
    assert_eq!(kept_at(root, pin, "HEAD", "ext/ctype").unwrap(), Ok(run));
    let now = Snapshot::disk(root).unwrap();
    assert_eq!(
        regressed(&now, "HEAD", pin, "ext/ctype").unwrap(),
        Against::Held
    );
}

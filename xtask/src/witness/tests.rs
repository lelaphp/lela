//! How one witness is judged, held beside the judge: the notes, the digest
//! asserted once per run, the streams and the status held apart, and what
//! `--accept` captures. What a run of the instrument writes is held in
//! `tests/witness.rs`, through `run`.
//!
//! The tests that run a snippet run it through the engine linked into the
//! harness, which is what the lane serves every witness through (harness
//! §7, revision 48; #401), with the pinned oracle beside it for the status
//! a fence pins. They return when the pinned binary is not on this
//! machine, unless the machine promised it (#38).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::fence;
use super::judge::{Run, held};
use super::process::Binary;
use super::{Ask, Judgement, Tally, capture, judge};
use crate::cfg::Target;
use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::oracle::{Observation, Promise};
use crate::scratch::Tree;

fn workspace() -> PathBuf {
    crate::built_root()
}

fn manifest() -> Manifest {
    Manifest::load(&workspace()).unwrap()
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// A source file of this test's own, in a scratch tree of its own. The
/// tree comes back with the file: it is what holds it there, and the
/// caller binds it for as long as it reads or writes the file.
fn source(name: &str, text: &str) -> (Tree, PathBuf) {
    let dir = Tree::new(&format!("witness-{name}")).unwrap();
    let file = dir.join("lib.rs");
    fs::write(&file, text).unwrap();
    (dir, file)
}

/// The workspace's `php`, for a fence marked `process`, and its
/// `php-cgi`, for one with a `request`; each built by the first test
/// that serves one.
fn binary() -> Binary {
    Binary::of(&workspace())
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// The witnesses of one text, judged as the lane judges them.
fn judge_text(text: &str, registry: &Registry) -> Tally {
    judged(Path::new("ext/fixture/src/lib.rs"), text, registry, false)
}

/// Judging decides and writes nothing; `--accept` is the caller's write, as
/// it is in the instrument. The run of record is the engine linked into the
/// harness, the lane's one adapter. The root is the workspace, which leaves
/// a file named by an absolute path where it stands.
fn judged(file: &Path, text: &str, registry: &Registry, accept: bool) -> Tally {
    let found = fence::extract(file, text);
    let ask = Ask {
        crates: Vec::new(),
        accept,
        staged: false,
        ..Ask::default()
    };
    let manifest = manifest();
    let tally = judge(
        &manifest.pin.oracle(),
        &binary(),
        registry,
        &ask,
        &found,
        None,
    )
    .unwrap();
    if accept {
        capture(&workspace(), &tally.captured).unwrap();
    }
    tally
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.witnesses.judged.len(), 1, "{tally:#?}");
    let judged = &tally.witnesses.judged[0];
    (judged.judgement, judged.note.clone())
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

#[test]
fn the_digest_is_asserted_once_over_a_run_of_three_witnesses() {
    if oracle().is_none() {
        return;
    }
    let one = |n: u8| {
        format!(
            "/// ```php\n/// echo {n};\n/// ```\n/// ```expect\n/// {n}\n/// ```\npub fn native{n}() {{}}\n"
        )
    };
    let text = format!("{}{}{}", one(1), one(2), one(3));
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    assert_eq!(found.witnesses.len(), 3, "{found:#?}");
    let manifest = manifest();
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let oracle = manifest.pin.oracle();
    let tally = judge(
        &oracle,
        &binary(),
        &registry(""),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap();
    assert_eq!(tally.witnesses.judged.len(), 3, "{tally:#?}");
    assert!(
        tally
            .witnesses
            .judged
            .iter()
            .all(|j| j.judgement == Judgement::Pass),
        "{tally:#?}"
    );
    assert_eq!(manifest.pin.digests_asserted(), 1);
}

/// Coverage first, the digest only before a snippet runs (#65): with no
/// binary at the pin's path the kernel without a witness still fails the
/// crate, the witness is a snippet that did not run rather than a
/// refusal, and the pin is never hashed.
#[test]
fn an_absent_oracle_leaves_the_coverage_half_judged_and_the_witness_unrun() {
    let text = format!(
        "{}/// A kernel without one.\npub fn kernel() {{}}\n",
        block("/// ```expect\n/// 1\n/// ```\n")
    );
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    assert_eq!(found.witnesses.len(), 1, "{found:#?}");
    assert_eq!(found.subjects.len(), 2, "{found:#?}");
    let mut manifest = manifest();
    manifest.pin.oracle.path = PathBuf::from("/no-php-here");
    let oracle = manifest.pin.oracle();
    let tally = judge(
        &oracle,
        &binary(),
        &registry(""),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap();
    assert!(tally.failed(), "{tally:#?}");
    assert_eq!(tally.missing.len(), 1, "{tally:#?}");
    assert!(tally.witnesses.judged.is_empty(), "{tally:#?}");
    assert_eq!(tally.witnesses.found, 1);
    assert_eq!(tally.witnesses.not_run(), 1);
    assert!(
        tally
            .oracle_absent
            .as_deref()
            .is_some_and(|why| why.contains("/no-php-here")),
        "{tally:#?}"
    );
    assert_eq!(
        manifest.pin.digests_asserted(),
        0,
        "nothing ran, so nothing hashed"
    );
}

#[test]
fn a_digest_mismatch_refuses_the_run_and_is_not_a_bork_per_witness() {
    if oracle().is_none() {
        return;
    }
    let found = fence::extract(
        Path::new("ext/fixture/src/lib.rs"),
        &block("/// ```expect\n/// 1\n/// ```\n"),
    );
    let mut manifest = manifest();
    manifest.pin.oracle.md5 = "0".repeat(32);
    let err = judge(
        &manifest.pin.oracle(),
        &binary(),
        &registry(""),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("the pin says"), "{err}");
}

#[test]
fn the_snippet_runs_as_witness_php_with_the_code_on_line_two() {
    if oracle().is_none() {
        return;
    }
    // php's own error text names the file and the line, which is what the
    // recipe's preamble is for (design §8). The oracle runs the snippet as
    // `%SCRATCH%/witness.php` and the linked engine, which serves the run
    // of record, names it `witness.php` (#293), and `%s` is one character
    // at least (run-tests.php's `[^\r\n]+`), so the pin both meet is
    // `%s 2`, the one the witness beside `magic_const` in php_compile
    // writes (#364).
    let text = format!(
        "/// ```php\n/// echo __FILE__, ' ', __LINE__;\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// %s 2\n/// ```\n"
    );
    let (judgement, note) = only(&judge_text(&text, &registry("")));
    assert_eq!(judgement, Judgement::Pass, "{note}");
}

#[test]
fn the_snippet_is_named_witness_php_as_the_recipe_names_it() {
    // The linked adapter serves the snippet under the recipe's
    // `script_name`, so a diagnostic's `in %s on line %d` names the file
    // the fresh-process adapter runs it as, and not `Standard input
    // code`, `serve_capture`'s name for a piped source (#293). The engine
    // compiles a served source under its name expanded, as php compiles
    // its primary file (#1569), so the name is the scratch path the
    // normaliser writes `%SCRATCH%` for, as the oracle's is.
    let recipe = manifest().pin.recipe;
    let snippet = "ini_set('display_errors', 'stderr');\necho $u, 'to the first';";
    let served = crate::oracle::linked::serve(&recipe, None, snippet).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&served.stderr),
        "Warning: Undefined variable $u in %SCRATCH%/witness.php on line 3"
    );
}

#[test]
fn the_linked_snippet_is_served_in_the_tree_its_file_stands_in() {
    // The linked adapter writes the snippet where the fresh-process
    // adapter writes it and serves it there, so `__DIR__`, a relative
    // `include` and the token the normaliser puts where the directory
    // was all read the one tree (#396). No oracle is asked: the run of
    // record is the linked engine's, and the token is the recipe's.
    let recipe = manifest().pin.recipe;
    let served = crate::oracle::linked::serve(&recipe, None, "echo __DIR__;").unwrap();
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        recipe.normalise.scratch_token
    );
}

/// A fence that names a regime of the pin runs under the recipe's ini and
/// that regime's, in both runs: the oracle's, whose status is what
/// `--accept` captures, and the linked engine's, which is the run of
/// record (harness §8, ledger 55). The recipe pins `short_open_tag=0` and
/// `[regime.short-tags]` turns it on, and the entry is
/// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so nothing the snippet itself can do
/// reaches it: a pass here is the regime reaching both.
#[test]
fn a_fence_that_names_a_regime_runs_under_the_recipes_ini_and_that_regimes() {
    if oracle().is_none() {
        return;
    }
    let snippet = "/// ```php\n/// ?>\n/// <? echo \"code\"; ?>\n/// ```\n";
    let under = |fences: &str| {
        only(&judge_text(
            &format!("{snippet}{fences}pub fn native() {{}}\n"),
            &registry(""),
        ))
    };
    // Under the recipe alone the tag is the text the file echoes.
    let (judgement, note) = under("/// ```expect\n/// <? echo \"code\"; ?>\n/// ```\n");
    assert_eq!(judgement, Judgement::Pass, "{note}");
    // Under the regime it opens code, in the oracle and in the engine.
    let (judgement, note) = under("/// ```expect regime=short-tags\n/// code\n/// ```\n");
    assert_eq!(judgement, Judgement::Pass, "{note}");
    // And the regime is what makes the difference, not the fence's body.
    let (judgement, _) =
        under("/// ```expect regime=short-tags\n/// <? echo \"code\"; ?>\n/// ```\n");
    assert_eq!(judgement, Judgement::Fail);
}

/// A regime is the pin's, so a fence naming one `oracle.toml` does not
/// carry is a block the lane cannot run: it is read off the pin before
/// anything runs, which is why the reading is the same with the pinned
/// binary here and absent, and the line names the regime and the ones the
/// pin has.
#[test]
fn a_fence_naming_a_regime_the_pin_does_not_carry_is_a_block_the_lane_cannot_run() {
    let text = format!(
        "{}pub fn native() {{}}\n",
        block("/// ```expect regime=no-such-regime\n/// 1\n/// ```\n")
    );
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    assert_eq!(found.witnesses.len(), 1, "{found:#?}");
    let mut manifest = manifest();
    for absent in [false, true] {
        if absent {
            manifest.pin.oracle.path = PathBuf::from("no-php-here");
        } else if oracle().is_none() {
            continue;
        }
        let tally = judge(
            &manifest.pin.oracle(),
            &binary(),
            &registry(""),
            &Ask::default(),
            &found,
            None,
        )
        .unwrap();
        assert!(tally.failed(), "absent={absent}: {tally:#?}");
        assert!(tally.witnesses.judged.is_empty(), "{tally:#?}");
        let [problem] = &tally.problems[..] else {
            panic!("{tally:#?}")
        };
        assert!(
            problem.contains("ext/fixture/src/lib.rs:1")
                && problem.contains("no-such-regime")
                && problem.contains("short-tags"),
            "{problem}"
        );
    }
}

/// A regime that carries `args` reaches a binary's command line (#1780,
/// #1887): the linked engine takes `-e` as a host builds it (#1822) and
/// no other, and a request host takes none, so a fence naming a regime
/// whose arguments cannot reach what serves it is a block the lane
/// cannot run unless it is marked `process`, read off the pin before
/// anything runs: the line names the regime, its arguments and the mark
/// that serves it. Marked `process`, the fence stands under the regime,
/// whose arguments the binary's run and the oracle's are both handed.
#[test]
fn a_fence_under_a_regime_of_args_is_served_where_they_reach_or_not_at_all() {
    let mut pin = manifest().pin;
    pin.regime.insert(
        "extended-info".to_owned(),
        crate::oracle::Regime {
            args: vec!["-e".to_owned()],
            ..crate::oracle::Regime::default()
        },
    );
    pin.regime.insert(
        "hide-args".to_owned(),
        crate::oracle::Regime {
            args: vec!["-H".to_owned()],
            ..crate::oracle::Regime::default()
        },
    );
    let witness = |fences: &str| {
        let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &block(fences));
        assert_eq!(found.witnesses.len(), 1, "{found:#?}");
        found.witnesses.into_iter().next().unwrap()
    };
    let linked = witness("/// ```expect regime=extended-info\n/// 1\n/// ```\n");
    let regime = super::under(&pin, &linked).unwrap().unwrap();
    assert_eq!(regime.args, ["-e"]);
    let hidden = witness("/// ```expect regime=hide-args\n/// 1\n/// ```\n");
    let request = witness(
        "/// ```request\n/// --GET--\n/// q=1\n/// ```\n/// ```expect regime=extended-info\n/// 1\n/// ```\n",
    );
    assert_eq!(request.served, fence::Served::Cgi);
    for (unmarked, name, arg) in [
        (&hidden, "hide-args", "\"-H\""),
        (&request, "extended-info", "\"-e\""),
    ] {
        let refused = super::under(&pin, unmarked).unwrap_err();
        assert!(
            refused.contains("ext/fixture/src/lib.rs:1")
                && refused.contains(&format!("[regime.{name}]"))
                && refused.contains(arg)
                && refused.contains("`process`"),
            "{refused}"
        );
    }
    let process = witness("/// ```expect process regime=hide-args\n/// 1\n/// ```\n");
    let regime = super::under(&pin, &process).unwrap().unwrap();
    assert_eq!(regime.args, ["-H"]);
    // A regime of ini alone is the linked engine's as it was.
    let short_tags = witness("/// ```expect regime=short-tags\n/// 1\n/// ```\n");
    assert!(super::under(&pin, &short_tags).unwrap().is_some());
}

#[test]
fn a_witness_that_differs_notes_the_difference_under_each_run_that_missed() {
    if oracle().is_none() {
        return;
    }
    let tally = judge_text(&block("/// ```expect\n/// 2\n/// ```\n"), &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
    assert!(note.contains("-2") && note.contains("+1"), "{note}");
    assert!(
        note.contains("not what the oracle ran") && note.contains("not what the linked engine ran"),
        "{note}"
    );
}

/// Two runs made up for [`held`], so the one rule both are held by is
/// read without php: the oracle's streams and status, then the engine's.
fn held_over(
    fences: &str,
    oracle: (&str, i32),
    engine: (&str, i32),
    rows: &str,
) -> (Judgement, String) {
    let text = block(fences);
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &text);
    let witness = &found.witnesses[0];
    let observed = |(stdout, exit): (&str, i32)| Observation {
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
        exit: Some(exit),
        wall: Duration::ZERO,
    };
    let (oracle, engine) = (observed(oracle), observed(engine));
    let rows = rows.replace("{id}", &witness.id());
    let judged = held(
        &manifest().pin,
        &registry(&rows),
        witness,
        witness.exit,
        [(Run::Oracle, &oracle), (Run::Engine, &engine)],
    );
    (judged.judgement, judged.note)
}

const ROW: &str = "[[divergence]]\nid = \"{id}\"\nkind = \"witness\"\nreason = \"A row a person wrote.\"\nstatus = \"accepted\"\nsince = \"2026-09-09\"\n";

/// The oracle's run is held to the fences as the engine's is (#783): a
/// fence the oracle does not meet fails however the engine ran, and the
/// note names the run that missed, by its stream or by its status.
#[test]
fn a_fence_the_oracle_does_not_meet_fails_and_names_the_oracle() {
    let fence = "/// ```expect\n/// 1\n/// ```\n";
    let (judgement, note) = held_over(fence, ("2", 0), ("1", 0), "");
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(
        note.contains("not what the oracle ran") && note.contains("+2"),
        "{note}"
    );
    assert!(!note.contains("linked engine"), "{note}");

    let (judgement, note) = held_over(fence, ("1", 255), ("1", 0), "");
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(
        note.contains("not what the oracle ran") && note.contains("+status 255"),
        "{note}"
    );

    let (judgement, note) = held_over(fence, ("1", 0), ("2", 0), "");
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(
        note.contains("not what the linked engine ran") && !note.contains("oracle"),
        "{note}"
    );

    assert_eq!(held_over(fence, ("1", 0), ("1", 0), "").0, Judgement::Pass);
}

/// The negative control of the rule (harness §4.1, #783): the fixture's
/// one fence pins `__FILE__` as the linked engine names the snippet, and
/// the oracle, which runs it by its scratch path, misses it.
#[test]
fn the_fixture_whose_fence_the_oracle_does_not_meet_fails_naming_the_oracle() {
    if oracle().is_none() {
        return;
    }
    let root = workspace().join("xtask/fixtures/witness/oracle-unmet");
    let fixture = Manifest::load(&root).unwrap();
    let found = super::extract(&root, &fixture.crates[0], None, Target::built()).unwrap();
    let tally = judge(
        &fixture.pin.oracle(),
        &binary(),
        &registry(""),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap();
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(note.contains("not what the oracle ran"), "{note}");
}

/// An accepted row names one side of a difference, the oracle's or the
/// engine's, so it accepts a fence one run misses and never one both miss.
#[test]
fn an_accepted_row_accepts_one_run_missing_the_fence_and_never_both() {
    let fence = "/// ```expect\n/// 1\n/// ```\n";
    for (oracle, engine) in [(("1", 0), ("2", 0)), (("2", 0), ("1", 0))] {
        let (judgement, note) = held_over(fence, oracle, engine, ROW);
        assert_eq!(judgement, Judgement::Diverged, "{note}");
        assert_eq!(note, "A row a person wrote.");
    }
    let (judgement, note) = held_over(fence, ("2", 0), ("3", 0), ROW);
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(
        note.contains("the oracle ran") && note.contains("the linked engine ran"),
        "{note}"
    );
}

#[test]
fn the_second_stream_is_handed_over_and_a_fence_is_held_apart_to_it() {
    if oracle().is_none() {
        return;
    }
    // A snippet whose diagnostic goes to the error stream under the
    // linked engine, which is `Capture`'s `stderr` handed over as the
    // fresh-process adapter hands php's fd 2 over (#248): a fence pinning
    // it passes, and one pinning nothing there fails on what was written,
    // an absent `stderr` fence asserting an empty second stream.
    let text = format!(
        "/// ```php\n/// ini_set('display_errors', 'stderr');\n/// echo $u, 'to the first';\n/// ```\n{}pub fn native() {{}}\n",
        "/// ```expect\n/// to the first\n/// ```\n/// ```stderr\n/// Warning: Undefined variable $u in %s on line 3\n/// ```\n"
    );
    let file = Path::new("ext/fixture/src/lib.rs");
    let tally = judged(file, &text, &registry(""), false);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Pass, "{note}");
    let without = text.replace(
        "/// ```stderr\n/// Warning: Undefined variable $u in %s on line 3\n/// ```\n",
        "",
    );
    let tally = judged(file, &without, &registry(""), false);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{note}");
    assert!(note.contains("Undefined variable $u"), "{note}");
}

#[test]
fn a_bare_exit_is_unpinned_until_accept_captures_the_status() {
    if oracle().is_none() {
        return;
    }
    // A snippet the engine ends on a status of its own: php's uncaught
    // error, which the engine writes byte for byte and both end 255 on
    // (`exit` is a construct the compiler does not read yet, so the
    // snippet that ended 3 is one the run of record cannot serve).
    let text = format!("/// ```php\n/// nope();\n/// ```\n{FATAL}pub fn native() {{}}\n");
    let text = text.as_str();
    let (_tree, file) = source("accept", text);

    let (judgement, note) = only(&judged(&file, text, &registry(""), false));
    assert_eq!(judgement, Judgement::Unpinned, "{note}");
    assert!(note.contains("--accept"), "{note}");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        text,
        "a run that does not accept writes nothing"
    );

    let tally = judged(&file, text, &registry(""), true);
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
    assert_eq!(tally.captured.len(), 1, "{tally:#?}");
    let written = fs::read_to_string(&file).unwrap();
    assert!(
        written.contains("```expect exit=255"),
        "the oracle's status is on the fence line: {written}"
    );

    // The witness now pins the status, so a second run accepts nothing.
    let again = judged(&file, &written, &registry(""), true);
    assert!(again.captured.is_empty(), "{again:#?}");
    assert_eq!(only(&again).0, Judgement::Pass);
}

#[test]
fn the_declared_durations_are_summed_and_gate_nothing() {
    if oracle().is_none() {
        return;
    }
    let tally = judge_text(
        &block("/// ```expect wall=250ms\n/// 1\n/// ```\n"),
        &registry(""),
    );
    assert_eq!(only(&tally).0, Judgement::Pass);
    assert_eq!(tally.wall.as_millis(), 250);
}

#[test]
fn a_block_the_grammar_cannot_read_fails_the_crate_where_it_stands() {
    let found = fence::extract(
        Path::new("ext/fixture/src/lib.rs"),
        "/// ```php\n/// echo 1;\n/// ```\n/// prose\npub fn native() {}\n",
    );
    let tally = judge(
        &manifest().pin.oracle(),
        &binary(),
        &Registry::default(),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap();
    assert!(tally.failed(), "{tally:#?}");
    assert_eq!(tally.problems.len(), 1, "{tally:#?}");
    assert!(
        tally.problems[0].contains("ext/fixture/src/lib.rs:1"),
        "{tally:#?}"
    );
}

/// A fence marked `process` is served by the `php` binary in a process
/// of its own and covered by what that process entered, a forked child's
/// request beside its parent's; the same snippet unmarked is served by
/// `php-linked` and forks that, never the harness (harness §8, revisions
/// 61 and 84; #1062, #2166).
#[test]
fn a_process_fence_forks_in_the_binary_and_never_in_the_harness() {
    if oracle().is_none() {
        return;
    }
    let snippet = "/// ```php\n/// $child = pcntl_fork(); if ($child === 0) { exit(3); }\n/// pcntl_waitpid($child, $status); var_dump(pcntl_wexitstatus($status));\n/// ```\n";
    let marked =
        format!("{snippet}/// ```expect process\n/// int(3)\n/// ```\npub fn native() {{}}\n");
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), &marked);
    let witness = &found.witnesses[0];
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    let (ran, entered) = super::judge::serve(&oracle, &binary(), witness, None).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&ran.stdout).trim_end(),
        "int(3)",
        "{ran:#?}"
    );
    let forks = entered
        .iter()
        .filter(|e| e.krate == "php_ext_pcntl" && e.name == "pcntl_fork")
        .count();
    assert_eq!(
        forks, 2,
        "the parent's request and the child's: {entered:#?}"
    );
    let (judgement, _) = only(&judge_text(&marked, &registry("")));
    assert_eq!(judgement, Judgement::Pass);

    // Unmarked, the fence forks `php-linked`, which carries pcntl as the
    // CLI does and serves one request at a time: its child ends with its
    // own request and never reads the next, so the fence passes there too,
    // and nothing forks the harness (harness §8, revision 84; #2166).
    let unmarked = marked.replace("expect process", "expect");
    let (judgement, note) = only(&judge_text(&unmarked, &registry("")));
    assert_eq!(judgement, Judgement::Pass, "{note}");
}

/// The set rustc prints for `--print cfg` over `triple`, which it prints
/// without the target's standard library installed.
fn printed(triple: &str) -> Target {
    let out = std::process::Command::new("rustc")
        .args(["--print", "cfg", "--target", triple])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Target::parse(&String::from_utf8(out.stdout).unwrap())
}

/// The fixture of `a_native_under_a_cfg_the_target_denies_is_no_subject`
/// read as a Linux build and as a Darwin one would read it (#1068): the
/// lane on CI is the first and the oracle's host the second, so each
/// counts the native its own `#[cfg]` has and neither counts the other's.
#[test]
fn a_linux_build_and_a_darwin_one_each_count_their_own_natives() {
    let root = workspace().join("xtask/fixtures/witness/anchor-cfg");
    let fixture = Manifest::load(&root).unwrap();
    let rule = super::coverage_rule(&fixture);
    for (triple, own) in [
        ("x86_64-unknown-linux-gnu", "fn fixture_linux"),
        ("aarch64-apple-darwin", "fn fixture_apple"),
    ] {
        let found = super::extract(&root, &fixture.crates[0], rule, &printed(triple)).unwrap();
        let coverage = found.anchored.expect("the attributes declare natives");
        let names: Vec<String> = coverage
            .subjects(&std::collections::HashMap::new())
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            names,
            [
                "fn fixture_everywhere",
                "fn fixture_unix",
                own,
                "fn fixture_feature"
            ],
            "{triple}"
        );
    }
}

/// A repository whose one commit holds an `xtask` crate of one line.
fn committed_xtask(name: &str) -> Tree {
    use crate::git::git;
    let dir = Tree::new(name).unwrap();
    fs::create_dir_all(dir.join("xtask/src")).unwrap();
    fs::write(dir.join("xtask/Cargo.toml"), "[package]\n").unwrap();
    fs::write(dir.join("xtask/src/lib.rs"), "a\n").unwrap();
    git(&dir, &["init", "-q"], None).unwrap();
    git(&dir, &["add", "--", "xtask"], None).unwrap();
    let unhooked = [
        "-c",
        "user.name=x",
        "-c",
        "user.email=x@x",
        "-c",
        "commit.gpgsign=false",
    ];
    let commit = [
        &unhooked[..],
        &["-c", "core.hooksPath=/var/empty", "commit", "-q", "-m", "x"],
    ]
    .concat();
    git(&dir, &commit, None).unwrap();
    dir
}

/// `--unmeasured` leaves out a crate whose pass was taken over its tree
/// now and measures it again once one byte of it moves: what the merge
/// check runs after pre-commit measured the staged crates (#1312).
#[test]
fn unmeasured_leaves_out_a_crate_with_a_pass_over_its_tree_now() {
    use crate::reading::{Reading, Status, Store};
    let dir = committed_xtask("witness-unmeasured");
    let manifest = manifest();
    let store = Store::at(dir.join("target/harness"));
    assert_eq!(
        super::unmeasured(&dir, &manifest, &store).unwrap(),
        ["xtask"]
    );

    let krate = manifest.crate_named("xtask").unwrap();
    let mut reading = Reading::for_crate(
        super::INSTRUMENT,
        krate,
        &crate::key::Snapshot::disk(&dir).unwrap(),
    )
    .unwrap();
    reading.status = Status::Pass;
    store.record(&[reading.clone()]).unwrap();
    assert_eq!(
        super::unmeasured(&dir, &manifest, &store).unwrap(),
        ["xtask"],
        "a witness reading that records no pin is taken under no recipe this run knows"
    );
    let reading = reading.under(&manifest.pin);
    store.record(&[reading]).unwrap();
    assert!(
        super::unmeasured(&dir, &manifest, &store)
            .unwrap()
            .is_empty()
    );

    fs::write(dir.join("xtask/src/lib.rs"), "b\n").unwrap();
    assert_eq!(
        super::unmeasured(&dir, &manifest, &store).unwrap(),
        ["xtask"]
    );
}

/// A witness reading is taken under the oracle's recipe, so a change to
/// `oracle.toml`'s `[recipe.env]` stales it as a change to its crate's tree
/// does (#1387): 84a55f66 added two variables there, an absolute
/// `memory_get_usage()` moved with them, and `--unmeasured` ran nothing.
/// The fixture's entry is laid under the tracked pin's `[recipe.env]`; the
/// reading taken under the pin without it is stale under the pin with it,
/// and a comment added to the pin stales nothing.
#[test]
fn unmeasured_measures_a_crate_again_once_the_recipe_changes() {
    use crate::oracle::Pin;
    use crate::reading::{Reading, Status, Store};
    let dir = committed_xtask("witness-unmeasured-recipe");
    let mut manifest = manifest();
    let store = Store::at(dir.join("target/harness"));
    let krate = manifest.crate_named("xtask").unwrap();
    let mut reading = Reading::for_crate(
        super::INSTRUMENT,
        krate,
        &crate::key::Snapshot::disk(&dir).unwrap(),
    )
    .unwrap();
    reading.status = Status::Pass;
    let reading = reading.under(&manifest.pin);
    store.record(std::slice::from_ref(&reading)).unwrap();
    assert!(
        super::unmeasured(&dir, &manifest, &store)
            .unwrap()
            .is_empty()
    );

    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tracked = fs::read_to_string(workspace.join("../oracle.toml")).unwrap();
    let commented = Pin::parse(&format!("# a person's note\n{tracked}")).unwrap();
    assert_eq!(
        commented.digest(),
        manifest.pin.digest(),
        "a comment is no value"
    );
    let entry = fs::read_to_string(workspace.join("fixtures/witness/recipe-env/env.toml")).unwrap();
    let changed = tracked.replacen("[recipe.env]\n", &format!("[recipe.env]\n{entry}"), 1);
    assert_ne!(changed, tracked, "the pin has a [recipe.env]");
    let tree = crate::key::Snapshot::disk(&dir)
        .unwrap()
        .key(&krate.path())
        .unwrap();
    manifest.pin = Pin::parse(&changed).unwrap();
    assert!(reading.fresh_for(&tree, reading.recipe.as_deref().unwrap()));
    assert!(
        !reading.fresh_for(&tree, manifest.pin.digest()),
        "the store keeps a reading taken before the change fresh after it"
    );
    assert_eq!(
        super::unmeasured(&dir, &manifest, &store).unwrap(),
        ["xtask"]
    );
}

/// With the pinned php absent, a `request fpm` fence the engine's php-fpm
/// fails is `Unserved`, as it is beside the oracle, so its native is no
/// witness short, and the reading names the serve's failure (#1535). The
/// fixture `request-empty`'s stand-in, which reads the request and closes
/// it with no response, stands in for the engine's `php-fpm`. The
/// control: a fence whose serve failed recorded nothing, which read the
/// fixture's one native as missing and named nothing.
#[test]
fn with_the_oracle_absent_a_fence_the_engine_did_not_serve_is_named_and_not_missing() {
    let root = workspace().join("xtask/fixtures/witness/request-empty");
    let mut manifest = Manifest::load(&root).unwrap();
    manifest.pin.oracle.path = root.join("no-php-here");
    let rule = super::coverage_rule(&manifest);
    let krate = manifest
        .crates
        .iter()
        .find(|krate| krate.name == "ext/pcre")
        .unwrap();
    let found = super::extract(&root, krate, rule, Target::built()).unwrap();
    let binary = Binary::with_fpm(&root, root.join("php-fpm"));
    let tally = judge(
        &manifest.pin.oracle(),
        &binary,
        &Registry::default(),
        &Ask::default(),
        &found,
        None,
    )
    .unwrap();
    assert!(tally.oracle_absent.is_some(), "{tally:#?}");
    assert_eq!(tally.subjects, 1, "{tally:#?}");
    assert!(
        tally.missing.is_empty(),
        "a serve that failed is no missing witness: {tally:#?}"
    );
    let [unserved] = tally.unserved.as_slice() else {
        panic!("{tally:#?}")
    };
    assert!(
        unserved.contains("lib.rs") && unserved.contains("the php-fpm binary did not serve it"),
        "{unserved}"
    );
    let reading = super::reading(
        &tally,
        &crate::key::Snapshot::disk(&root).unwrap(),
        krate,
        rule,
    )
    .unwrap();
    assert!(
        reading.message.contains("1 fence the engine did not serve")
            && reading
                .message
                .contains("the php-fpm binary did not serve it"),
        "{}",
        reading.message
    );
    assert!(
        !reading.message.contains("without a witness"),
        "{}",
        reading.message
    );
}

/// A fence naming a locale the host knows to neither run is a skip, as a
/// `.phpt` whose `--SKIPIF--` dies on `!setlocale(...)` is (#1678): not
/// run, so its pinned bytes, which nothing prints, neither pass nor fail
/// it; named in the note, counted in the reading as `skipped`, and the
/// crate's reading does not fail on it. The control: the same fence
/// naming `C`, which every host knows, runs and fails on the same bytes,
/// and one naming both is a skip for the one the host lacks.
#[test]
fn a_fence_naming_a_locale_the_host_lacks_is_a_counted_skip() {
    if oracle().is_none() {
        return;
    }
    let pins = |locales: &str| {
        block(&format!(
            "/// ```expect {locales}\n/// what nothing prints\n/// ```\n"
        ))
    };
    let tally = judge_text(&pins("locale=xx_XX.NOPE"), &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Skipped, "{note}");
    assert!(
        note.contains("setlocale(LC_ALL, \"xx_XX.NOPE\")") && note.contains("locale=xx_XX.NOPE"),
        "{note}"
    );
    assert!(!tally.failed(), "{tally:#?}");
    assert!(tally.missing.is_empty(), "{tally:#?}");

    let (judgement, note) = only(&judge_text(&pins("locale=C"), &registry("")));
    assert_eq!(judgement, Judgement::Fail, "{note}");
    let (judgement, note) = only(&judge_text(
        &pins("locale=C locale=xx_XX.NOPE"),
        &registry(""),
    ));
    assert_eq!(judgement, Judgement::Skipped, "{note}");

    let root = workspace();
    let manifest = manifest();
    let krate = manifest
        .crates
        .iter()
        .find(|krate| krate.name == "ext/pcre")
        .unwrap();
    let reading = super::reading(
        &tally,
        &crate::key::Snapshot::disk(&root).unwrap(),
        krate,
        super::coverage_rule(&manifest),
    )
    .unwrap();
    assert_eq!(reading.status, crate::reading::Status::Skip, "{reading:#?}");
    assert_eq!(reading.numbers["skipped"].to_string(), "1", "{reading:#?}");
    assert_eq!(reading.numbers["failed"].to_string(), "0", "{reading:#?}");
    assert!(
        reading
            .message
            .contains("1 witness skipped for a locale this host lacks"),
        "{}",
        reading.message
    );
}

/// A locale the oracle knows is run and held as any fence is: the probe
/// keeps nothing of the host's locale for the snippet, which runs under
/// the recipe's.
#[test]
fn a_fence_naming_a_locale_the_host_knows_runs_and_is_held() {
    if oracle().is_none() {
        return;
    }
    let tally = judge_text(
        &block("/// ```expect locale=C\n/// 1\n/// ```\n"),
        &registry(""),
    );
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

/// A pass over `ext/reflection` taken before a change to `php_engine`,
/// a crate it is built from, does not stand after it: `--unmeasured` runs
/// it again with every crate above `php_engine`, as cargo's fingerprint
/// rebuilds them. A change to `ext/ctype` alone runs `ext/ctype` alone
/// (#1318, `xtask/fixtures/closure`).
#[test]
fn unmeasured_measures_a_crate_again_once_a_crate_it_is_built_from_changes() {
    use crate::reading::{Reading, Status, Store};
    let dir = crate::key::fixture("witness-unmeasured");
    let manifest = manifest();
    let store = Store::at(dir.join("target/harness"));
    let crates = ["php_engine", "php_ext", "ext/reflection", "ext/ctype"];
    let pass_all = || {
        for name in crates {
            let krate = manifest.crate_named(name).unwrap();
            let mut reading = Reading::for_crate(
                super::INSTRUMENT,
                krate,
                &crate::key::Snapshot::disk(&dir).unwrap(),
            )
            .unwrap();
            reading.status = Status::Pass;
            store.record(&[reading.under(&manifest.pin)]).unwrap();
        }
    };
    pass_all();
    assert!(
        super::unmeasured(&dir, &manifest, &store)
            .unwrap()
            .is_empty()
    );

    fs::write(
        dir.join("crates/php_engine/src/lib.rs"),
        "pub fn execute() { }\n",
    )
    .unwrap();
    let mut left = super::unmeasured(&dir, &manifest, &store).unwrap();
    left.sort();
    assert_eq!(
        left,
        ["ext/ctype", "ext/reflection", "php_engine", "php_ext"]
    );

    pass_all();
    fs::write(
        dir.join("ext/ctype/src/lib.rs"),
        "pub fn ctype_digit() {}\n",
    )
    .unwrap();
    assert_eq!(
        super::unmeasured(&dir, &manifest, &store).unwrap(),
        ["ext/ctype"]
    );
}

/// A full volume is no witness that borked (#644): the trouble that
/// carries [`crate::scratch::Full`] leaves the judge as the error the
/// crate's reading says as a skip naming the volume, and any other trouble
/// is the witness's bork, its whole sentence kept.
#[test]
fn a_witness_the_volume_had_no_room_for_is_no_bork() {
    let text =
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn native() {}\n";
    let found = fence::extract(Path::new("ext/fixture/src/lib.rs"), text);
    let witness = &found.witnesses[0];
    let tree = Tree::new("witness-full").unwrap();
    let full = crate::scratch::made(
        std::io::Error::from_raw_os_error(nix::errno::Errno::ENOSPC as i32),
        &tree.join("oracle.lock-taking"),
        "opening the scratch lock",
    )
    .context("the oracle did not run it");
    let full = judge::borked(witness, full).expect_err("a full volume is not a verdict");
    assert!(crate::scratch::Full::of(&full).is_some(), "{full:#}");

    let other = anyhow::anyhow!("php ended").context("the oracle did not run it");
    let judged = judge::borked(witness, other).unwrap();
    assert_eq!(judged.judgement, Judgement::Bork);
    assert_eq!(judged.note, "the oracle did not run it: php ended");
}

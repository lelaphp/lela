//! The lane from inside: which files a corpus holds and what it names
//! them, what one file holds to dump, the verdict on two dumps, and the
//! words of a reading at each rung. What a run of the whole lane writes is
//! held from outside, in `tests/compile_identity.rs`, through `run`.
//!
//! Nothing here spawns php: the verdict is over bytes, and the negative
//! control is the fixture pair under `xtask/fixtures/compile-identity`,
//! php's own dump beside one that differs from it by a line.

use std::fs;
use std::path::{Path, PathBuf};

use super::dump::{Dumped, Folded, Held, engine_side, held, verdict};
use super::*;
use crate::divergence::Registry;
use crate::oracle::Runner;
use crate::reading::Status;
use crate::scratch::Tree;
use crate::tally::Judged as OneJudged;

fn fixtures() -> PathBuf {
    crate::package_dir().join("fixtures/compile-identity")
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// A corpus of this test's own, the files written into a scratch tree
/// named for the test. The tree comes back with it: it is what holds the
/// files there, and the caller binds it for as long as it reads them.
fn corpus(name: &str, files: &[(&str, &str)]) -> (Tree, Corpus) {
    let dir = Tree::new(&format!("identity-{name}")).unwrap();
    for (file, text) in files {
        fs::write(dir.join(file), text).unwrap();
    }
    let corpus = Corpus::at(&dir).unwrap();
    (dir, corpus)
}

/// A tally of these judgements over a corpus of `found` files.
fn tally(found: usize, went: &[Judgement]) -> Tally {
    Tally {
        judged: went
            .iter()
            .enumerate()
            .map(|(i, went)| {
                OneJudged::new(
                    &format!("compile:c/{i}.phpt"),
                    &format!("{i}.phpt"),
                    *went,
                    "",
                )
            })
            .collect(),
        found,
    }
}

fn engine() -> Runner {
    Runner::Engine(PathBuf::from("target/debug/php"))
}

#[test]
fn the_per_commit_corpus_is_the_three_directories_design_6_1_names() {
    let tree = Path::new("/pinned/php-src");
    let corpus = Corpus::per_commit(tree);
    assert_eq!(corpus.scope, CORPUS);
    assert_eq!(
        corpus.dirs,
        ["Zend/tests", "tests", "sapi/cli"].map(PathBuf::from)
    );
    // A file is named and keyed by where it stands in the corpus, never
    // by the path this machine keeps the tree at: a registry row a person
    // writes names the corpus.
    let file = tree.join("Zend/tests/generators/yield.phpt");
    assert_eq!(corpus.name(&file), "Zend/tests/generators/yield.phpt");
    assert_eq!(
        corpus.id(&file),
        "compile:php-src/Zend/tests/generators/yield.phpt"
    );
    // The tree is on no machine here, so all three directories are absent
    // and the reading would say which.
    assert_eq!(corpus.absent().len(), 3);
}

#[test]
fn a_corpus_holds_the_phpt_and_the_php_beside_them_and_nothing_else() {
    let (_tree, corpus) = corpus(
        "files",
        &[
            ("b.phpt", "--TEST--\nb\n--FILE--\n<?php\n--EXPECT--\n"),
            ("a.php", "<?php\n"),
            ("notes.txt", "not a program"),
            ("c.inc", "<?php\n"),
        ],
    );
    let named: Vec<String> = corpus
        .files()
        .unwrap()
        .iter()
        .map(|file| corpus.name(file))
        .collect();
    assert_eq!(named, ["a.php", "b.phpt"]);
}

#[test]
fn what_one_file_holds_to_dump_is_the_file_section_or_the_program_itself() {
    let (_tree, held_in) = corpus(
        "held",
        &[
            (
                "test.phpt",
                "--TEST--\nOne\n--FILE--\n<?php echo 1;\n--EXPECT--\n1\n",
            ),
            ("plain.php", "<?php echo 2;\n"),
            (
                "redirect.phpt",
                "--TEST--\nElsewhere\n--REDIRECTTEST--\nreturn [];\n",
            ),
            ("broken.phpt", "no --TEST-- line at all\n"),
        ],
    );
    let dir = held_in.root;
    match held(&dir.join("test.phpt")) {
        Held::Program(body) => assert_eq!(body, b"<?php echo 1;\n"),
        other => panic!("{}", why(&other)),
    }
    match held(&dir.join("plain.php")) {
        Held::Program(body) => assert_eq!(body, b"<?php echo 2;\n"),
        other => panic!("{}", why(&other)),
    }
    // A test that holds no program of its own is skipped with its reason,
    // and one that is no test at all borks: neither is a file the
    // compiler was measured on, and neither passes.
    assert!(matches!(
        held(&dir.join("redirect.phpt")),
        Held::NoProgram(_)
    ));
    assert!(matches!(
        held(&dir.join("broken.phpt")),
        Held::Unreadable(_)
    ));
}

fn why(held: &Held) -> String {
    match held {
        Held::Program(body) => String::from_utf8_lossy(body).into_owned(),
        Held::NoProgram(why) | Held::Unreadable(why) => why.clone(),
    }
}

/// The fixture that must fail (harness §12 item 4): php's own dump beside
/// one that differs from it by a line is not a pass, and the note carries
/// both lines.
#[test]
fn a_dump_that_differs_from_the_oracles_by_one_line_is_not_a_pass() {
    let dir = fixtures();
    let expected = fs::read(dir.join("oracle.dump")).unwrap();
    let actual = fs::read(dir.join("engine.dump")).unwrap();
    let judged = verdict(
        &registry(""),
        "compile:corpus/identity.phpt",
        "identity.phpt",
        &Dumped::of(&expected),
        &Dumped::of(&actual),
    );
    assert_eq!(judged.judgement, Judgement::Fail, "{judged:#?}");
    assert!(
        judged.note.contains("-0002 T2 = MUL CV1($b) int(2)")
            && judged.note.contains("+0002 T2 = MUL CV1($b) int(3)"),
        "{}",
        judged.note
    );
    // The same bytes are the pass, so the fixture fails for the line and
    // for nothing else.
    assert_eq!(
        verdict(
            &registry(""),
            "compile:corpus/identity.phpt",
            "identity.phpt",
            &Dumped::of(&expected),
            &Dumped::of(&expected),
        )
        .judgement,
        Judgement::Pass
    );
}

/// The one door a difference is accepted through is the registry, here as
/// everywhere (harness §4.3).
#[test]
fn an_accepted_row_of_kind_compile_is_what_makes_a_difference_met() {
    let accepted = registry(
        r#"
[[divergence]]
id = "compile:corpus/identity.phpt"
kind = "compile"
reason = "A person's sentence."
status = "accepted"
since = "2026-09-11"
"#,
    );
    let judged = verdict(
        &accepted,
        "compile:corpus/identity.phpt",
        "identity.phpt",
        &Dumped::of(b"$_main:\n0000 RETURN null\n"),
        &Dumped::of(b"$_main:\n0000 RETURN int(1)\n"),
    );
    assert_eq!(judged.judgement, Judgement::Diverged);
    assert_eq!(judged.note, "A person's sentence.");
}

/// Every rung's message states the denominator: what a person reads of
/// this lane is how much of the corpus is identical, and a message
/// without the count of files says nothing about the compiler's distance.
#[test]
fn every_reading_states_the_denominator() {
    let root = crate::built_root();
    let (_tree, corpus) = corpus("reading", &[]);
    let red = tally(3, &[Judgement::Fail, Judgement::Fail, Judgement::Fail]);
    let red = reading(
        &red,
        &crate::key::Snapshot::disk(&root).unwrap(),
        &corpus,
        &engine(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(red.instrument, INSTRUMENT);
    assert_eq!(red.status, Status::Fail);
    assert!(red.message.contains("3 of 3 files differ"), "{red:#?}");

    let green = tally(2, &[Judgement::Pass, Judgement::Pass]);
    let green = reading(
        &green,
        &crate::key::Snapshot::disk(&root).unwrap(),
        &corpus,
        &engine(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(green.status, Status::Pass);
    assert!(green.message.contains("2 of 2"), "{green:#?}");

    // Nothing dumped: the files are found and counted, and the reading is
    // a skip that says why, never a pass over a corpus nothing measured.
    let none = reading(
        &Tally::found(6288),
        &crate::key::Snapshot::disk(&root).unwrap(),
        &corpus,
        &engine(),
        None,
        Some("the pinned php is not at /nowhere"),
    )
    .unwrap();
    assert_eq!(none.status, Status::Skip);
    assert!(
        none.message.contains("no file of 6288 dumped") && none.message.contains("/nowhere"),
        "{none:#?}"
    );
}

/// The pinned oracle standing where the engine belongs keeps a reading of
/// its own and never displaces the engine's: a control of the lane.
#[test]
fn the_control_is_scoped_apart_from_the_engines_reading() {
    let root = crate::built_root();
    let (_tree, corpus) = corpus("control", &[]);
    let control = Runner::Control(PathBuf::from("/pinned/php"));
    let control = reading(
        &tally(1, &[Judgement::Pass]),
        &crate::key::Snapshot::disk(&root).unwrap(),
        &corpus,
        &control,
        None,
        None,
    )
    .unwrap();
    assert_eq!(control.scope, format!("{}@control", corpus.scope));
    // A run under a regime beside `dump` keeps a reading of its own too,
    // its control's after the control's mark (#1822).
    let under = |runner: &Runner| {
        reading(
            &tally(1, &[Judgement::Pass]),
            &crate::key::Snapshot::disk(&root).unwrap(),
            &corpus,
            runner,
            Some("extended-info"),
            None,
        )
        .unwrap()
        .scope
    };
    assert_eq!(
        under(&engine()),
        format!("{}@regime=extended-info", corpus.scope)
    );
    assert_eq!(
        under(&Runner::Control(PathBuf::from("/pinned/php"))),
        format!("{}@control@regime=extended-info", corpus.scope)
    );
    assert_eq!(control.status, Status::Skip);
    assert!(
        control.message.contains("control of the lane"),
        "{control:#?}"
    );
}

/// A file php compiled nothing from is no measurement of the compiler: two
/// empty dumps are two compilers that both stopped, and a pass there would
/// count every unparsable file of the corpus as green against an engine
/// that compiles nothing at all.
#[test]
fn two_empty_dumps_are_a_skip_and_never_a_pass() {
    let nothing = Dumped {
        arrays: Vec::new(),
        said: b"Parse error: syntax error, unexpected token \"{\" in program.php on line 2\n"
            .to_vec(),
    };
    let judged = verdict(
        &registry(""),
        "compile:corpus/unparsable.phpt",
        "unparsable.phpt",
        &nothing,
        &Dumped::of(b""),
    );
    assert_eq!(judged.judgement, Judgement::Skip, "{judged:#?}");
    assert!(judged.note.contains("Parse error"), "{}", judged.note);
    // An engine that dumps where php dumped nothing still differs.
    let dumped = verdict(
        &registry(""),
        "compile:corpus/unparsable.phpt",
        "unparsable.phpt",
        &nothing,
        &Dumped::of(b"$_main:\n0000 RETURN null\n"),
    );
    assert_eq!(dumped.judgement, Judgement::Fail, "{dumped:#?}");
}

/// A file named from outside the corpus keeps its own name: the path this
/// machine holds it at is no part of a key a person writes a row against.
#[test]
fn a_file_outside_the_corpus_is_named_by_its_own_name() {
    let corpus = Corpus::per_commit(Path::new("/pinned/php-src"));
    let elsewhere = Path::new("/somewhere/else/one.phpt");
    assert_eq!(corpus.name(elsewhere), "one.phpt");
    assert_eq!(corpus.id(elsewhere), "compile:php-src/one.phpt");
}

/// The engine's side of a file it finished no dump of is that file's
/// verdict and never the run's end (design §8): a dump past the recipe's
/// limit is the file's `fail` naming the limit (#699), one a signal ended
/// is the file's `fail` naming the signal (#1069), and a binary that would
/// not run is the file's `bork`.
#[test]
fn a_dump_past_the_limit_or_ended_by_a_signal_is_the_files_fail_naming_it() {
    let past = crate::oracle::PastLimit {
        binary: PathBuf::from("target/lanes/php"),
        script: PathBuf::from("/s/identity.php"),
        limit_s: 1,
    };
    let (judgement, why) = engine_side(Err(past.into())).unwrap_err();
    assert_eq!(judgement, Judgement::Fail);
    assert!(why.contains("past the recipe's limit of 1 s"), "{why}");

    let died = crate::oracle::Observation {
        stdout: Vec::new(),
        stderr: b"op arrays, half of them".to_vec(),
        exit: crate::oracle::Ended::Signal(11),
        wall: std::time::Duration::ZERO,
    };
    let (judgement, why) = engine_side(Ok(died)).unwrap_err();
    assert_eq!(judgement, Judgement::Fail);
    assert!(why.contains("died on signal 11"), "{why}");

    let (judgement, why) = engine_side(Err(anyhow::anyhow!("no such file"))).unwrap_err();
    assert_eq!(judgement, Judgement::Bork);
    assert!(why.contains("no such file"), "{why}");

    let dumped = crate::oracle::Observation {
        stdout: b"No syntax errors detected".to_vec(),
        stderr: b"$_main:".to_vec(),
        exit: crate::oracle::Ended::Status(0),
        wall: std::time::Duration::ZERO,
    };
    assert_eq!(engine_side(Ok(dumped)).unwrap().arrays, b"$_main:");
}

/// The two values a binary folds from its own build, as it says them.
fn folds(binary: &str, build_date: &str) -> Folded {
    Folded::said(format!("{binary}\0{build_date}").as_bytes()).unwrap()
}

/// A fixture dump of a program that names both constants, as php's
/// `zend_try_ct_eval_const` folds them, and returns `returned`.
fn naming_both(binary: &str, build_date: &str, returned: &str) -> Dumped {
    Dumped::of(
        format!(
            "$_main:
     ; (lines=5, args=0, vars=0, tmps=1)
     ; (before optimizer)
     ; %SCRATCH%/build_date.php:1-4
     ; return  [] RANGE[0..0]
0000 INIT_FCALL 1 96 string(\"var_dump\")
0001 SEND_VAL string(\"{binary}\") 1
0002 DO_ICALL
0003 ECHO string(\"{build_date}\")
0004 RETURN {returned}
"
        )
        .as_bytes(),
    )
}

const PHP: &str = "/pinned/php/bin/php";
const ENGINE: &str = "/work/dax-php/target/release-da/php";

/// Each side's dump with that binary's own values put out of it, then the
/// verdict: what `Dumps::judge` does with the two dumps of one file.
fn compared(expected: Dumped, actual: Dumped) -> OneJudged<Judgement> {
    verdict(
        &registry(""),
        "compile:php-src/tests/basic/build_date.phpt",
        "tests/basic/build_date.phpt",
        &expected.placed(Some(&folds(PHP, "Jul 28 2026 13:06:52"))),
        &actual.placed(Some(&folds(ENGINE, "Oct 11 2026 09:14:03"))),
    )
}

/// #2692: two dumps that differ in each binary's own `PHP_BINARY` and
/// `PHP_BUILD_DATE` alone are identical once each binary's own values are
/// put out of its dump, and no registry row is consulted to say so.
#[test]
fn each_binarys_own_build_literals_alone_are_no_difference() {
    let judged = compared(
        naming_both(PHP, "Jul 28 2026 13:06:52", "int(1)"),
        naming_both(ENGINE, "Oct 11 2026 09:14:03", "int(1)"),
    );
    assert_eq!(judged.judgement, Judgement::Pass, "{judged:#?}");
}

/// The placeholder hides the literal and nothing else: one more
/// difference in the file is still the file's failure, named.
#[test]
fn a_difference_beside_the_build_literals_still_differs() {
    let judged = compared(
        naming_both(PHP, "Jul 28 2026 13:06:52", "int(1)"),
        naming_both(ENGINE, "Oct 11 2026 09:14:03", "int(2)"),
    );
    assert_eq!(judged.judgement, Judgement::Fail, "{judged:#?}");
    assert!(
        judged.note.contains("-0004 RETURN int(1)") && judged.note.contains("+0004 RETURN int(2)"),
        "{}",
        judged.note
    );
}

/// Only a binary's own value is put out: a compiler that folds a value
/// other than its own, here php's path and date, still differs. Its own
/// value is put out of a literal a folded concatenation made of it too,
/// `PHP_BINARY . ' -S …'` as `php_cli_server_ipv4_error_message.phpt`
/// folds it, and the rest of that literal is still compared.
#[test]
fn a_binary_that_folds_a_value_not_its_own_still_differs() {
    let judged = compared(
        naming_both(PHP, "Jul 28 2026 13:06:52", "int(1)"),
        naming_both(PHP, "Jul 28 2026 13:06:52", "int(1)"),
    );
    assert_eq!(judged.judgement, Judgement::Fail, "{judged:#?}");
    assert!(
        judged
            .note
            .contains("-0001 SEND_VAL string(\"<PHP_BINARY>\") 1")
            && judged
                .note
                .contains(&format!("+0001 SEND_VAL string(\"{PHP}\") 1"))
            && judged
                .note
                .contains("-0003 ECHO string(\"<PHP_BUILD_DATE>\")"),
        "{}",
        judged.note
    );
    let serving = |binary: &str, port: u16| {
        Dumped::of(
            format!("0002 SEND_VAL string(\"{binary} -S \\\"192.168.1.999:{port}\\\"\") 1\n")
                .as_bytes(),
        )
    };
    assert_eq!(
        compared(serving(PHP, 8080), serving(ENGINE, 8080)).judgement,
        Judgement::Pass
    );
    assert_eq!(
        compared(serving(PHP, 8080), serving(ENGINE, 8081)).judgement,
        Judgement::Fail
    );
}

/// The value is matched as php's dump prints it, escaped as
/// `zend_dump_const` escapes a string, and only inside a string literal:
/// the same bytes on a line the dump writes of its own are left, and a
/// dump with no answer beside it is compared as it stands.
#[test]
fn the_value_is_matched_as_php_prints_it_inside_a_literal_alone() {
    let odd = folds("/a \"b\"\\c\n\u{7f}", "d");
    let dump = Dumped::of(
        b"     ; /a \\\"b\\\"\\\\c\\n\\x7F:1-2\n0000 SEND_VAL string(\"/a \\\"b\\\"\\\\c\\n\\x7F\") 1\n",
    );
    assert_eq!(
        dump.clone().placed(Some(&odd)).arrays,
        b"     ; /a \\\"b\\\"\\\\c\\n\\x7F:1-2\n0000 SEND_VAL string(\"<PHP_BINARY>\") 1\n"
    );
    assert_eq!(dump.clone().placed(None), dump);
}

/// A binary's answer is heard only whole: two values one NUL apart, each
/// of them something, since an empty value would match every literal.
#[test]
fn only_a_whole_answer_is_heard() {
    for answer in [&b""[..], b"/bin/php", b"/bin/php\0", b"\0date", b"a\0b\0c"] {
        assert_eq!(Folded::said(answer), None, "{answer:?}");
    }
}

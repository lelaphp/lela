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

use super::dump::{Dumped, Held, held, verdict};
use super::*;
use crate::divergence::Registry;
use crate::oracle::Runner;
use crate::reading::Status;
use crate::tally::Judged as OneJudged;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/compile-identity")
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// A corpus of this test's own, the files written into a directory named
/// for the test.
fn corpus(name: &str, files: &[(&str, &str)]) -> Corpus {
    let dir = std::env::temp_dir().join(format!("xtask-identity-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for (file, text) in files {
        fs::write(dir.join(file), text).unwrap();
    }
    Corpus::at(&dir).unwrap()
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
    let corpus = corpus(
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
    let dir = corpus(
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
    )
    .root;
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
    let corpus = corpus("reading", &[]);
    let red = tally(3, &[Judgement::Fail, Judgement::Fail, Judgement::Fail]);
    let red = reading(&red, &root, &corpus, &engine(), None).unwrap();
    assert_eq!(red.instrument, INSTRUMENT);
    assert_eq!(red.status, Status::Fail);
    assert!(red.message.contains("3 of 3 files differ"), "{red:#?}");

    let green = tally(2, &[Judgement::Pass, Judgement::Pass]);
    let green = reading(&green, &root, &corpus, &engine(), None).unwrap();
    assert_eq!(green.status, Status::Pass);
    assert!(green.message.contains("2 of 2"), "{green:#?}");

    // Nothing dumped: the files are found and counted, and the reading is
    // a skip that says why, never a pass over a corpus nothing measured.
    let none = reading(
        &Tally::found(6288),
        &root,
        &corpus,
        &engine(),
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
    let corpus = corpus("control", &[]);
    let control = Runner::Control(PathBuf::from("/pinned/php"));
    let control = reading(
        &tally(1, &[Judgement::Pass]),
        &root,
        &corpus,
        &control,
        None,
    )
    .unwrap();
    assert_eq!(control.scope, format!("{}@control", corpus.scope));
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

//! The lane from inside: the verdict on two renderings, the negative
//! control under `xtask/fixtures/crosscheck/`, and which extensions a run
//! holds. What a run writes is held from outside, in
//! `tests/crosscheck.rs`, through `run`. Nothing here spawns php.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::manifest::Crate;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/crosscheck")
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

#[test]
fn the_fixture_is_the_oracles_own_rendering_and_the_one_byte_apart_is_not_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("engine.re")).unwrap();
    assert_eq!(
        oracle.len(),
        engine.len(),
        "the fixture differs by a byte, not a length"
    );
    assert_eq!(
        judge(
            &registry(""),
            "ctype",
            Column::Re,
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass
    );
    let Judgement::Fail(note) = judge(
        &registry(""),
        "ctype",
        Column::Re,
        &oracle,
        &Rendering::Bytes(engine),
    ) else {
        panic!("one byte apart is a pass")
    };
    assert!(
        note.starts_with(
            "the engine's rendering differs from the oracle's at line 4, 2 lines in all:\n"
        ),
        "{note}"
    );
    assert!(
        note.contains("-    Function [ <internal:ctype> function ctype_alnum ] {")
            && note.contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{note}"
    );
}

/// The note names the first differing line and the hunk there, not the
/// whole diff: a line the oracle has and the engine lacks is `-` alone,
/// and the count is over every changed line, later hunks included.
#[test]
fn the_note_is_the_first_differing_line_and_the_count_of_all_of_them() {
    let diff = " same\n same\n-oracle only\n same\n-theirs\n+ours\n";
    assert_eq!(
        first_difference(diff),
        "the engine's rendering differs from the oracle's at line 3, 3 lines in all:\n-oracle only"
    );
    let added = " same\n+engine only\n same\n";
    assert_eq!(
        first_difference(added),
        "the engine's rendering differs from the oracle's at line 2, 1 line in all:\n+engine only"
    );
    assert!(first_difference(" same\n").contains("no line shows"));
    let long = " head\n-a\n-b\n-c\n-d\n-e\n-f\n same\n";
    assert_eq!(
        first_difference(long),
        "the engine's rendering differs from the oracle's at line 2, 6 lines in all:\n-a\n-b\n-c\n-d\n… and 2 more lines of this hunk"
    );
}

/// The engine's side is php's own call: `echo` over a `ReflectionExtension`
/// is `__toString()`, what `php --re` prints, and `info()` is
/// `php_info_print_module`, what `php --ri` prints. A name is a PHP
/// string literal whatever it holds.
#[test]
fn the_program_is_echo_over_a_reflection_extension_and_the_name_is_a_literal() {
    assert_eq!(
        Column::Re.program("ctype"),
        "echo new ReflectionExtension('ctype');"
    );
    assert_eq!(
        Column::Ri.program("ctype"),
        "(new ReflectionExtension('ctype'))->info();"
    );
    assert_eq!(
        Column::Re.program("it's\\"),
        "echo new ReflectionExtension('it\\'s\\\\');"
    );
    assert_eq!(
        Column::Ri.program("it's\\"),
        "(new ReflectionExtension('it\\'s\\\\'))->info();"
    );
}

/// An extension php registers under another name than its crate's is
/// asked for by php's name on both sides, the oracle's flag and the
/// engine's reflector, while its reading keeps the crate's scope:
/// `ext/opcache` is `Zend OPcache` (run-tests.php:2908-2916), and the
/// same table lowers `-m`'s listing back to the crate for `--EXTENSIONS--`.
#[test]
fn a_crate_php_names_otherwise_is_rendered_under_the_module_name() {
    assert_eq!(
        Column::Re.program("opcache"),
        "echo new ReflectionExtension('Zend OPcache');"
    );
    assert_eq!(Column::Ri.scope("opcache"), "ext/opcache/ri");
    assert_eq!(crate::oracle::module_name("ctype"), "ctype");
    assert_eq!(crate::oracle::crate_name("Zend OPcache"), "opcache");
    assert_eq!(crate::oracle::crate_name("SPL"), "spl");
}

/// The `--ri` column (#273): php's own `--ri ctype` standing for the
/// engine's is a pass, and the one a byte apart, `Enabled` for `enabled`
/// on line 3, is not; the fixture carries no load number to renumber.
#[test]
fn the_ri_fixture_is_the_oracles_own_rendering_and_the_one_byte_apart_is_not_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.ri")).unwrap();
    let engine = fs::read(fixtures().join("engine.ri")).unwrap();
    assert_eq!(oracle.len(), engine.len(), "one byte, not a length");
    assert_eq!(
        oracle, b"ctype\n\nctype functions => enabled",
        "php_info_print_module in text mode, trimmed"
    );
    assert_eq!(
        judge(
            &registry(""),
            "ctype",
            Column::Ri,
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass
    );
    let Judgement::Fail(note) = judge(
        &registry(""),
        "ctype",
        Column::Ri,
        &oracle,
        &Rendering::Bytes(engine),
    ) else {
        panic!("one byte apart is a pass")
    };
    assert!(
        note.starts_with(
            "the engine's rendering differs from the oracle's at line 3, 2 lines in all:\n"
        ) && note.contains("-ctype functions => enabled")
            && note.contains("+ctype functions => Enabled"),
        "{note}"
    );
}

/// Each column has its flag, its kept extension, its scope and its
/// divergence key: the `--re` reading keeps the crate's name, the scope
/// `done` holds a default extension to, and the `--ri` one stands beside
/// it under `/ri`; an accepted row for one column accepts nothing for the
/// other.
#[test]
fn the_two_columns_have_their_own_flag_scope_and_key() {
    assert_eq!(Column::ALL, [Column::Re, Column::Ri]);
    assert_eq!(Column::Re.flag(), "--re");
    assert_eq!(Column::Ri.flag(), "--ri");
    assert_eq!(Column::Re.kept(), "re");
    assert_eq!(Column::Ri.kept(), "ri");
    assert_eq!(Column::Re.scope("ctype"), "ext/ctype");
    assert_eq!(Column::Ri.scope("ctype"), "ext/ctype/ri");
    assert_eq!(Column::Re.id("ctype"), "crosscheck:ctype");
    assert_eq!(Column::Ri.id("ctype"), "crosscheck:ctype/ri");
    let oracle = fs::read(fixtures().join("oracle.ri")).unwrap();
    let engine = fs::read(fixtures().join("engine.ri")).unwrap();
    let accepted = registry(
        r#"
[[divergence]]
id = "crosscheck:ctype"
kind = "crosscheck"
reason = "the --re row (fixture)."
status = "accepted"
since = "2026-09-18"
"#,
    );
    assert!(
        matches!(
            judge(
                &accepted,
                "ctype",
                Column::Ri,
                &oracle,
                &Rendering::Bytes(engine)
            ),
            Judgement::Fail(_)
        ),
        "a row for the --re column accepts nothing for --ri"
    );
}

/// The load number (#317): the rendering with `#10` standing as `#7` is
/// the oracle's own under the pin's `extension_number_token` row, since
/// both streams go through [`oracle::normalise`] before the verdict, and
/// the same bytes against a pin without the row are not.
#[test]
fn the_renumbered_fixture_is_a_pass_under_the_row_and_not_without_it() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("renumbered.re")).unwrap();
    assert!(
        oracle.starts_with(b"Extension [ <persistent> extension #10 ctype")
            && engine.starts_with(b"Extension [ <persistent> extension #7 ctype"),
        "the fixture differs by its load number"
    );
    let with_row = Manifest::load(Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap())
        .unwrap()
        .pin
        .recipe
        .normalise;
    assert!(
        with_row.extension_number_token.is_some(),
        "the pin carries the row"
    );
    let mut without_row = with_row.clone();
    without_row.extension_number_token = None;

    let under = |rules: &oracle::Normalise| {
        judge(
            &registry(""),
            "ctype",
            Column::Re,
            &oracle::normalise(rules, oracle.clone()),
            &Rendering::Bytes(oracle::normalise(rules, engine.clone())),
        )
    };
    assert_eq!(under(&with_row), Judgement::Pass);
    let Judgement::Fail(note) = under(&without_row) else {
        panic!("the load number is compared without the row")
    };
    assert!(
        note.contains("-Extension [ <persistent> extension #10 ctype")
            && note.contains("+Extension [ <persistent> extension #7 ctype"),
        "{note}"
    );
}

/// The registry's `crosscheck` kind (#318): a difference an accepted row
/// names under `crosscheck:<name>` is `Diverged` with the row's reason and
/// a pass; the same row with status `open`, or a row for another
/// extension, accepts nothing.
#[test]
fn a_difference_an_accepted_row_names_is_diverged_and_a_pass() {
    let oracle = fs::read(fixtures().join("oracle.re")).unwrap();
    let engine = fs::read(fixtures().join("engine.re")).unwrap();
    let row = |status: &str| {
        format!(
            r#"
[[divergence]]
id = "crosscheck:ctype"
kind = "crosscheck"
reason = "ctype_alnun is the fixture's one byte (fixture)."
status = "{status}"
since = "2026-09-18"
"#
        )
    };
    let accepted = registry(&row("accepted"));
    let judged = judge(
        &accepted,
        "ctype",
        Column::Re,
        &oracle,
        &Rendering::Bytes(engine.clone()),
    );
    assert_eq!(
        judged,
        Judgement::Diverged("ctype_alnun is the fixture's one byte (fixture).".to_owned())
    );
    assert_eq!(judged.status(), Status::Pass);
    assert_eq!(
        judge(
            &accepted,
            "ctype",
            Column::Re,
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass,
        "the same bytes are a pass, row or no row"
    );
    assert!(
        matches!(
            judge(
                &accepted,
                "hash",
                Column::Re,
                &oracle,
                &Rendering::Bytes(engine.clone())
            ),
            Judgement::Fail(_)
        ),
        "a row for ctype accepts nothing for hash"
    );
    assert!(
        matches!(
            judge(
                &registry(&row("open")),
                "ctype",
                Column::Re,
                &oracle,
                &Rendering::Bytes(engine)
            ),
            Judgement::Fail(_)
        ),
        "an open row accepts nothing"
    );
}

#[test]
fn an_absent_rendering_fails_saying_the_engine_rendered_nothing() {
    let judged = judge(
        &registry(""),
        "ctype",
        Column::Re,
        b"x",
        &Rendering::Absent("not built".to_owned()),
    );
    assert_eq!(judged.status(), Status::Fail);
    let Judgement::Fail(note) = judged else {
        panic!("{judged:?}")
    };
    assert_eq!(note, "the engine rendered nothing: not built");
}

#[test]
fn the_default_set_is_every_ext_row_outside_a_feature_and_ext_names_one() {
    let manifest = Manifest::of([
        Crate::named("php_heap"),
        Crate::named("ext/ctype"),
        Crate::named("ext/mbstring").with_feature("mbstring"),
        Crate::named("ext/hash"),
    ]);
    assert_eq!(extensions(&manifest, &Ask::default()), ["ctype", "hash"]);
    let one = Ask {
        ext: vec!["mbstring".to_owned()],
        ..Ask::default()
    };
    assert_eq!(extensions(&manifest, &one), ["mbstring"]);
}

#[test]
fn the_oracles_bytes_are_kept_under_the_scope_and_the_digest() {
    let store = Store::at("/store");
    assert_eq!(
        kept(&store, "abc", "ctype", Column::Re),
        PathBuf::from("/store/crosscheck/ext/ctype/abc.re")
    );
    assert_eq!(
        kept(&store, "abc", "ctype", Column::Ri),
        PathBuf::from("/store/crosscheck/ext/ctype/abc.ri")
    );
}

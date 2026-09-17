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
            &oracle,
            &Rendering::Bytes(oracle.clone())
        ),
        Judgement::Pass
    );
    let Judgement::Fail(note) = judge(&registry(""), "ctype", &oracle, &Rendering::Bytes(engine))
    else {
        panic!("one byte apart is a pass")
    };
    assert!(
        note.contains("-    Function [ <internal:ctype> function ctype_alnum ] {"),
        "{note}"
    );
    assert!(
        note.contains("+    Function [ <internal:ctype> function ctype_alnun ] {"),
        "{note}"
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
    assert_eq!(scope("ctype"), "ext/ctype");
    assert_eq!(id("ctype"), "crosscheck:ctype");
}

#[test]
fn the_oracles_bytes_are_kept_under_the_scope_and_the_digest() {
    let store = Store::at("/store");
    assert_eq!(
        kept(&store, "abc", "ctype"),
        PathBuf::from("/store/crosscheck/ext/ctype/abc.re")
    );
}

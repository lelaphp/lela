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

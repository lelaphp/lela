//! The ICU major `build.rs` reads from the pin (`build/pin.rs`, #4406),
//! held to fixture pins: the bump that re-freezes the closure on
//! `icu4c@79` asks for 79, a key the reading does not know is no concern
//! of it, and a pin that names no one ICU keg is refused in the build's
//! words.

#[path = "../build/pin.rs"]
mod pin;

use pin::{Major, Refusal, icu_major};

/// A pin as `oracle.toml` is laid out, its closure being `closure`, with
/// a key under `[oracle]` the reading does not know.
fn pin_with(closure: &str) -> String {
    format!(
        "[oracle]\nversion = \"8.5.13\"\nformula = \"php@8.5\"\n\n\
         [closure]\ncurl = \"8.23.0\"\n{closure}zstd = \"1.5.7_1\"\n\n\
         [source]\ntag = \"php-8.5.13\"\n"
    )
}

/// The closure re-frozen at the bump names `icu4c@79`, and the build asks
/// for ICU 79 by that keg.
#[test]
fn a_pin_naming_icu4c_79_asks_for_79() {
    let major = icu_major(&pin_with("\"icu4c@79\" = \"79.1\"\n"));
    assert_eq!(major, Ok(Major(79)));
    assert_eq!(major.map(Major::keg), Ok("icu4c@79".to_owned()));
}

/// The workspace's pin names one ICU keg, the one the build links today.
#[test]
fn the_workspace_pin_names_one_icu() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../oracle.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let major = icu_major(&text).unwrap();
    assert!(text.contains(&format!("\"{}\" = \"{major}.", major.keg())));
}

/// A pin with no `[closure]` names no ICU.
#[test]
fn a_pin_with_no_closure_is_refused() {
    let refusal = icu_major("[oracle]\nversion = \"8.5.12\"\n").unwrap_err();
    assert_eq!(refusal, Refusal::NoClosure);
    assert_eq!(
        refusal.to_string(),
        "oracle.toml has no [closure], whose icu4c@<major> keg names the ICU the oracle links"
    );
}

/// A closure with no `icu4c@<major>` keg pins no ICU; an unversioned
/// `icu4c` names no major and is not one.
#[test]
fn a_closure_with_no_icu_keg_is_refused() {
    for closure in ["", "icu4c = \"79.1\"\n"] {
        let refusal = icu_major(&pin_with(closure)).unwrap_err();
        assert_eq!(refusal, Refusal::NoKeg);
        assert_eq!(
            refusal.to_string(),
            "oracle.toml's [closure] names no icu4c@<major> keg, so it pins no ICU for the oracle"
        );
    }
}

/// A closure with two ICU kegs is refused naming both, since the oracle
/// links one.
#[test]
fn a_closure_with_two_icu_kegs_is_refused() {
    let closure = "\"icu4c@79\" = \"79.1\"\n\"icu4c@78\" = \"78.3\"\n";
    let refusal = icu_major(&pin_with(closure)).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "oracle.toml's [closure] names 2 ICU kegs, icu4c@78 and icu4c@79, where the oracle links one"
    );
}

/// A keg whose name's major is not a number is refused naming it.
#[test]
fn a_keg_whose_major_is_not_a_number_is_refused() {
    let refusal = icu_major(&pin_with("\"icu4c@next\" = \"80.0\"\n")).unwrap_err();
    assert_eq!(
        refusal.to_string(),
        "oracle.toml's [closure] names icu4c@next, whose major is not a number"
    );
}

/// A pin that is not TOML is refused with the parser's words.
#[test]
fn a_pin_that_is_not_toml_is_refused() {
    let refusal = icu_major("[closure\n").unwrap_err();
    assert!(
        refusal
            .to_string()
            .starts_with("oracle.toml does not parse: "),
        "{refusal}"
    );
}

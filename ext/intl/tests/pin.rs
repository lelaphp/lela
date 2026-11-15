//! What ICU `build.rs` links (`build/pin.rs`), held to fixture pins and
//! to versions made up for `pkg-config` to have found: the bump that
//! re-freezes the closure on `icu4c@79` asks for 79, a key the reading
//! does not know is no concern of it, a pin that names no one ICU keg is
//! refused in the build's words, and on macOS an ICU of another
//! major than the pin's, or none, is refused naming the keg, where any
//! other target links what it finds at or above php's floor.

#[path = "../build/pin.rs"]
mod pin;

use pin::{Asked, Major, Refusal, Unlinked, held_to_pin, icu_major};

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
    let pin: toml::Table = text.parse().unwrap();
    let version = pin["closure"][&major.keg()].as_str().unwrap();
    assert!(version.starts_with(&format!("{major}.")), "{version}");
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

/// What the build asks on each target: the pin's keg on macOS alone.
fn asked(target_os: &str, pin: &str) -> Asked {
    if held_to_pin(target_os) {
        Asked::Keg(icu_major(pin).unwrap())
    } else {
        Asked::Floor
    }
}

/// Over a fixture pin naming ICU 79 where `pkg-config` finds 78.3, the
/// build refuses on macOS naming the keg the pin asks for, and on any
/// other target links 78 and hands that major to the binding.
#[test]
fn an_icu_of_another_major_than_the_pin_is_refused_on_macos_alone() {
    let pin = pin_with("\"icu4c@79\" = \"79.1\"\n");
    let refusal = asked("macos", &pin).linked(Some("78.3")).unwrap_err();
    assert_eq!(
        refusal,
        Unlinked::OtherMajor {
            found: "78.3".to_owned(),
            keg: Major(79)
        }
    );
    assert_eq!(
        refusal.to_string(),
        "pkg-config icu-uc: found ICU 78.3, where the pin asks for icu4c@79: put its lib/pkgconfig ahead on PKG_CONFIG_PATH (Homebrew: `$(brew --prefix icu4c@79)/lib/pkgconfig`)"
    );
    assert_eq!(asked("macos", &pin).linked(Some("79.1")), Ok(Major(79)));
    for os in ["linux", "freebsd"] {
        assert_eq!(asked(os, &pin).linked(Some("78.3")), Ok(Major(78)));
    }
}

/// With no ICU found the build refuses on every target: on macOS naming
/// the keg the pin asks for, elsewhere naming php's floor.
#[test]
fn a_missing_icu_is_refused_naming_what_the_build_asks_for() {
    let pin = pin_with("\"icu4c@79\" = \"79.1\"\n");
    assert_eq!(
        asked("macos", &pin).linked(None).unwrap_err().to_string(),
        "pkg-config icu-uc: no ICU found; the pin asks for icu4c@79: put its lib/pkgconfig on PKG_CONFIG_PATH (Homebrew: `$(brew --prefix icu4c@79)/lib/pkgconfig`)"
    );
    assert_eq!(
        asked("linux", &pin).linked(None).unwrap_err().to_string(),
        "pkg-config icu-uc: no ICU found; php's intl asks for ICU 57.1 or later: put its lib/pkgconfig on PKG_CONFIG_PATH"
    );
}

/// Away from macOS the floor is php's, `icu-uc >= 57.1`, held component
/// by component as `pkg-config --atleast-version` holds it.
#[test]
fn away_from_macos_an_icu_below_phps_floor_is_refused() {
    for found in ["57.1", "57.1.0", "57.2", "60", "74.2", "80.0.1"] {
        let major = found.split('.').next().unwrap().parse().unwrap();
        assert_eq!(
            Asked::Floor.linked(Some(found)),
            Ok(Major(major)),
            "{found}"
        );
    }
    for found in ["57", "57.0", "56.1", "4.8.1"] {
        assert_eq!(
            Asked::Floor.linked(Some(found)),
            Err(Unlinked::BelowFloor(found.to_owned()))
        );
    }
    assert_eq!(
        Asked::Floor.linked(Some("56.1")).unwrap_err().to_string(),
        "pkg-config icu-uc: found ICU 56.1, below the 57.1 php's intl asks for: put its lib/pkgconfig ahead on PKG_CONFIG_PATH"
    );
}

/// A version that is not dotted numbers is refused on every target,
/// rather than read for a major it may not have.
#[test]
fn a_version_that_is_not_dotted_numbers_is_refused() {
    for asked in [Asked::Keg(Major(78)), Asked::Floor] {
        for found in ["", "78.rc1", "seventy-eight"] {
            assert_eq!(
                asked.linked(Some(found)),
                Err(Unlinked::NotAVersion(found.to_owned()))
            );
        }
    }
}

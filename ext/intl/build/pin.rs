//! What ICU the build links, and the major it hands to the binding.
//!
//! On macOS that is the ICU the pin's `[closure]` names, the one
//! `icu4c@<major>` keg among the kegs the pinned binaries load (#4406),
//! and an ICU of another major, or none, is refused naming that keg. On
//! any other target it is the ICU `pkg-config` finds, at or above php's
//! own floor, `icu-uc >= 57.1` (`build/php.m4`'s `PHP_SETUP_ICU`), as
//! php built on that host links it (#4428). Included by `build.rs`, which
//! asks `pkg-config` and reads the pin, and by `tests/pin.rs`, which holds
//! both readings to fixture pins and made-up versions. Only `[closure]`'s
//! key names are read, so a key of the pin this does not know is no
//! concern of the build's.

use std::fmt;

/// The formula name of Homebrew's ICU, before the `@<major>` of its
/// versioned kegs.
const FORMULA: &str = "icu4c";

/// The major version of the ICU the pin's closure names, the one in its
/// keg's name and in every C symbol ICU exports (`u_errorName_<major>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Major(pub(crate) u32);

impl Major {
    /// The keg the pin names it by, `icu4c@<major>`.
    pub(crate) fn keg(self) -> String {
        format!("{FORMULA}@{}", self.0)
    }
}

impl fmt::Display for Major {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Why the pin names no one ICU major, each the build's refusal in its
/// words ([`fmt::Display`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The pin does not parse as TOML; the parser's words.
    Unreadable(String),
    /// The pin has no `[closure]` table.
    NoClosure,
    /// The closure names no `icu4c@<major>` keg.
    NoKeg,
    /// The closure names a keg `icu4c@<x>` whose `<x>` is not a number.
    NotAMajor(String),
    /// The closure names two kegs or more, sorted.
    Kegs(Vec<String>),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Unreadable(why) => write!(f, "oracle.toml does not parse: {why}"),
            Refusal::NoClosure => write!(
                f,
                "oracle.toml has no [closure], whose {FORMULA}@<major> keg names the ICU the oracle links"
            ),
            Refusal::NoKeg => write!(
                f,
                "oracle.toml's [closure] names no {FORMULA}@<major> keg, so it pins no ICU for the oracle"
            ),
            Refusal::NotAMajor(keg) => write!(
                f,
                "oracle.toml's [closure] names {keg}, whose major is not a number"
            ),
            Refusal::Kegs(kegs) => write!(
                f,
                "oracle.toml's [closure] names {} ICU kegs, {}, where the oracle links one",
                kegs.len(),
                kegs.join(" and ")
            ),
        }
    }
}

/// The ICU major the pin `text` names: the one key of its `[closure]`
/// that is `icu4c@<major>`, or the refusal that says why there is none.
pub(crate) fn icu_major(text: &str) -> Result<Major, Refusal> {
    let pin: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| Refusal::Unreadable(e.message().to_owned()))?;
    let Some(toml::Value::Table(closure)) = pin.get("closure") else {
        return Err(Refusal::NoClosure);
    };
    let prefix = format!("{FORMULA}@");
    let mut kegs: Vec<(&String, &str)> = (closure.keys())
        .filter_map(|keg| Some((keg, keg.strip_prefix(&prefix)?)))
        .collect();
    kegs.sort();
    match kegs.as_slice() {
        [] => Err(Refusal::NoKeg),
        [(keg, major)] => (major.parse())
            .map(Major)
            .map_err(|_| Refusal::NotAMajor((*keg).clone())),
        more => Err(Refusal::Kegs(
            more.iter().map(|(k, _)| (*k).clone()).collect(),
        )),
    }
}

/// php's floor for ICU, `icu-uc >= 57.1` (`build/php.m4`'s
/// `PHP_SETUP_ICU`), held as `pkg-config --atleast-version` holds a
/// dotted version, component by component.
const FLOOR: [u32; 2] = [57, 1];

/// Whether the build for `target_os` (`CARGO_CFG_TARGET_OS`) is held to
/// the pin's ICU: on macOS, where the pin's `[closure]` names the kegs
/// the oracle loads, and nowhere else (#4428).
pub(crate) fn held_to_pin(target_os: &str) -> bool {
    target_os == "macos"
}

/// What the build asks of the ICU `pkg-config` finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Asked {
    /// On macOS, the major of the keg the pin's closure names.
    Keg(Major),
    /// On any other target, php's floor, [`FLOOR`].
    Floor,
}

/// Why the build links no ICU, each the build's refusal in its words
/// ([`fmt::Display`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unlinked {
    /// `pkg-config icu-uc` found nothing.
    Missing(Asked),
    /// It found a version that is not dotted numbers.
    NotAVersion(String),
    /// On macOS, it found an ICU of another major than the pin's keg.
    OtherMajor { found: String, keg: Major },
    /// On any other target, it found an ICU below php's floor.
    BelowFloor(String),
}

impl Asked {
    /// The major to hand to the binding, of the ICU whose version
    /// `pkg-config --modversion icu-uc` printed (`found`, none where it
    /// found no ICU), or why the build links none.
    pub(crate) fn linked(self, found: Option<&str>) -> Result<Major, Unlinked> {
        let Some(found) = found else {
            return Err(Unlinked::Missing(self));
        };
        let parts: Option<Vec<u32>> = found.split('.').map(|p| p.parse().ok()).collect();
        let Some(&first) = parts.as_deref().and_then(<[u32]>::first) else {
            return Err(Unlinked::NotAVersion(found.to_owned()));
        };
        let major = Major(first);
        match self {
            Asked::Keg(keg) if keg != major => Err(Unlinked::OtherMajor {
                found: found.to_owned(),
                keg,
            }),
            Asked::Floor if parts.as_deref() < Some(FLOOR.as_slice()) => {
                Err(Unlinked::BelowFloor(found.to_owned()))
            }
            Asked::Keg(_) | Asked::Floor => Ok(major),
        }
    }

    /// Where to put the ICU asked for, `ahead` of another where one was
    /// found: the keg's `lib/pkgconfig` for the pin's, by Homebrew's name.
    fn put(self, f: &mut fmt::Formatter<'_>, ahead: &str) -> fmt::Result {
        write!(f, "put its lib/pkgconfig {ahead}on PKG_CONFIG_PATH")?;
        match self {
            Asked::Keg(major) => write!(
                f,
                " (Homebrew: `$(brew --prefix {})/lib/pkgconfig`)",
                major.keg()
            ),
            Asked::Floor => Ok(()),
        }
    }
}

/// php's floor as `pkg-config` spells it, `57.1`.
fn floor() -> String {
    let parts: Vec<String> = FLOOR.iter().map(u32::to_string).collect();
    parts.join(".")
}

impl fmt::Display for Unlinked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pkg-config icu-uc: ")?;
        match self {
            Unlinked::Missing(asked @ Asked::Keg(major)) => {
                write!(f, "no ICU found; the pin asks for {}: ", major.keg())?;
                asked.put(f, "")
            }
            Unlinked::Missing(asked @ Asked::Floor) => {
                let floor = floor();
                write!(
                    f,
                    "no ICU found; php's intl asks for ICU {floor} or later: "
                )?;
                asked.put(f, "")
            }
            Unlinked::NotAVersion(found) => {
                write!(f, "found ICU {found}, which is not a dotted version")
            }
            Unlinked::OtherMajor { found, keg } => {
                let asked = Asked::Keg(*keg);
                write!(
                    f,
                    "found ICU {found}, where the pin asks for {}: ",
                    keg.keg()
                )?;
                asked.put(f, "ahead ")
            }
            Unlinked::BelowFloor(found) => {
                let floor = floor();
                write!(
                    f,
                    "found ICU {found}, below the {floor} php's intl asks for: "
                )?;
                Asked::Floor.put(f, "ahead ")
            }
        }
    }
}

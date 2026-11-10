//! The ICU major the pin asks for, read from `oracle.toml`'s `[closure]`:
//! the one `icu4c@<major>` keg among the kegs the pinned binaries load
//! (#4406). Included by `build.rs`, which holds the ICU `pkg-config` finds
//! to it, and by `tests/pin.rs`, which holds this reading to fixture pins.
//! Only `[closure]`'s key names are read, so a key of the pin this does
//! not know is no concern of the build's.

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

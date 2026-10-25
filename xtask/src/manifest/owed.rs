//! `[[owed]]`: the instruments a crate owes a fresh passing reading of
//! before `done` (harness-design.md §4.5 condition 1), one row per
//! instrument, as Mitchell ruled on #3184 and #3206 built. Which crates
//! owe it is the row's: a glob or a list of them and, where the row names
//! one, a predicate the harness derives in code ([`When`]), the one way a
//! row narrows (#3278: it fails closed, so a new crate owes the row).
//! A row carrying `exempt` is refused as any unknown key is. The instrument ([`Instrument`]) and the predicate are
//! decoded here and nowhere else, and a pattern that names no crate is
//! refused, so a misspelt row cannot drop an obligation unseen.

use anyhow::{Result, bail};
use serde::Deserialize;

use super::{Crate, OneOrMany, segment_matches};

/// One `[[owed]]` row: the crates `crates` names owe a reading of `instrument` where `when` holds of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owed {
    pub instrument: Instrument,
    /// The crates, by manifest name; `*` stands for any run of a name's
    /// characters, a `/` among them, so `"*"` is every crate and
    /// `"ext/*"` every extension.
    pub crates: OneOrMany,
    /// What else must hold of a crate, derived in code.
    pub when: Option<When>,
}

/// An instrument that writes a reading at a crate's scope, which `done`
/// reads, by the name its readings carry: one of [`OWABLE`], the only
/// names an `[[owed]]` row decodes to. One that writes none there,
/// `status`, `permit` or `track`, would stay unmet for good, so it is
/// refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Instrument(&'static str);

/// The instruments a row may name, each module's `INSTRUMENT`.
const OWABLE: [&str; 13] = [
    crate::clippy::INSTRUMENT,
    crate::test::INSTRUMENT,
    crate::witness::INSTRUMENT,
    crate::api_check::INSTRUMENT,
    crate::budget::INSTRUMENT,
    crate::unsafe_audit::INSTRUMENT,
    crate::phpt::INSTRUMENT,
    crate::port_map::INSTRUMENT,
    crate::crosscheck::INSTRUMENT,
    crate::miri::INSTRUMENT,
    crate::vocab::INSTRUMENT,
    crate::makala::compare::INSTRUMENT,
    crate::makala::soak::INSTRUMENT,
];

impl Instrument {
    /// The name its readings carry.
    pub fn name(self) -> &'static str {
        self.0
    }
}

impl<'de> Deserialize<'de> for Instrument {
    fn deserialize<D: serde::Deserializer<'de>>(named: D) -> Result<Instrument, D::Error> {
        let name = String::deserialize(named)?;
        let found = OWABLE.into_iter().find(|owable| *owable == name);
        found.map(Instrument).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "[[owed]] names the instrument {name:?}, which writes no reading of a crate `done` reads; a row names one of {}",
                OWABLE.join(", ")
            ))
        })
    }
}

/// A predicate an `[[owed]]` row names by its `when` and the harness
/// derives, each from what the instrument writes its readings over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum When {
    /// The per-commit `phpt` lane runs a subset of the crate: it owns one,
    /// and the binary under test carries it or its row names an oracle
    /// variant (`done::measured_by_the_lane`).
    PhptSubset,
    /// The crate `api.lock` lives with, the one scope `port-map` writes
    /// (`port_map::home_of_the_lock`).
    LockHome,
    /// The binary under test carries the crate (`Carried::carries`), the
    /// extensions the per-commit lane renders.
    Carried,
    /// The crate is the engine's: every crate but the harness
    /// ([`of_the_engine`]), as #3278 ruled.
    Engine,
}

/// Whether `krate` is the engine's: every crate but the harness,
/// [`crate::layout::HARNESS`], which measures the engine and carries no
/// kernel, native or witness of its own. [`When::Engine`]'s predicate,
/// stated here once, which the witness instrument reads as well.
pub(crate) fn of_the_engine(krate: &Crate) -> bool {
    krate.name != crate::layout::HARNESS
}

impl Owed {
    /// Whether the crate named `name` is among the row's `crates`.
    pub fn covers(&self, name: &str) -> bool {
        self.crates
            .iter()
            .any(|pattern| segment_matches(pattern, name))
    }
}

/// The rows as the grammar holds them: each instrument named by one row
/// alone, and each pattern of `crates` naming a crate of the manifest's
/// `crates`.
pub(super) fn hold(rows: &[Owed], crates: &[Crate]) -> Result<()> {
    for (n, row) in rows.iter().enumerate() {
        let instrument = row.instrument.name();
        if rows[..n].iter().any(|r| r.instrument == row.instrument) {
            bail!("[[owed]] names {instrument:?} twice; one row states what it is owed");
        }
        let mut patterns = row.crates.iter();
        let named = |p: &str| crates.iter().any(|k| segment_matches(p, &k.name));
        if let Some(pattern) = patterns.find(|p| !named(p)) {
            bail!("an [[owed]] row for {instrument} names {pattern:?}, which matches no crate");
        }
    }
    Ok(())
}

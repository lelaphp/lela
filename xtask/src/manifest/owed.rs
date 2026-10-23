//! `[[owed]]`: the instruments a crate owes a fresh passing reading of
//! before `done` (harness-design.md §4.5 condition 1), one row per
//! instrument, as Mitchell ruled on #3184 and #3206 built. Which crates
//! owe it is the row's: a glob or a list of them, less its `exempt`, and,
//! where the row names one, a predicate the harness derives in code
//! ([`When`]), decoded here and nowhere else.

use anyhow::{Result, bail};
use clap::Subcommand as _;
use serde::Deserialize;

use super::{OneOrMany, segment_matches};
use crate::cli::Command;

/// One `[[owed]]` row: the crates `crates` names, less `exempt`, owe a
/// reading of `instrument` where `when` holds of them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owed {
    /// An `xtask` instrument, by its subcommand's name.
    pub instrument: String,
    /// The crates, by manifest name; `*` stands for any run of a name's
    /// characters, a `/` among them, so `"*"` is every crate and
    /// `"ext/*"` every extension.
    pub crates: OneOrMany,
    /// The crates among them that owe nothing, globbed as `crates` is.
    #[serde(default)]
    pub exempt: Vec<String>,
    /// What else must hold of a crate, derived in code.
    pub when: Option<When>,
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
}

impl Owed {
    /// Whether the crate named `name` is among the row's `crates` and
    /// none of its `exempt`.
    pub fn covers(&self, name: &str) -> bool {
        let named = |pattern: &str| segment_matches(pattern, name);
        self.crates.iter().any(named) && !self.exempt.iter().any(|p| named(p))
    }
}

/// The rows as the grammar holds them: each instrument an `xtask`
/// subcommand, and named by one row alone.
pub(super) fn hold(rows: &[Owed]) -> Result<()> {
    for (n, Owed { instrument, .. }) in rows.iter().enumerate() {
        if !Command::has_subcommand(instrument) {
            bail!("[[owed]] names the instrument {instrument:?}, which is no `xtask` subcommand");
        }
        if rows[..n].iter().any(|r| r.instrument == *instrument) {
            bail!("[[owed]] names {instrument:?} twice; one row states what it is owed");
        }
    }
    Ok(())
}

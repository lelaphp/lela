//! `[review]`: what makes a diff review class A, as Mitchell ruled in
//! Round 3 (Q27 (c)): a diff touching `php_heap` or `php_process`, or one
//! that changes a non-comment line naming `unsafe`; every other diff, an
//! xtask-only one among them, is class B. The table lives here and not in a
//! skill file or a constant (Q28 (a)), since the class sets the review
//! coverage's denominator and an agent must not be able to narrow it: the
//! grammar lands first, and a person steers the row in. An absent table is
//! absent, never defaulted, and the review module reads it as no diff being
//! class A, saying so wherever it renders a class.
//!
//! The crates are named as the manifest names them and each must be a
//! `[[crate]]` row, so a misspelt name cannot leave a crate out of the
//! class unseen; a table that makes nothing class A is refused for the
//! same reason.

use anyhow::{Result, bail};
use serde::Deserialize;

use super::Crate;

/// The `[review]` table: the crates a diff makes class A by touching, and
/// whether a changed `unsafe` line does.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    /// The crates, by manifest name, a change under any of which makes a
    /// diff class A.
    pub crates: Vec<String>,
    /// Whether a changed line of Rust that names `unsafe` outside a
    /// comment or a string makes a diff class A.
    pub unsafe_lines: bool,
}

/// Hold the table to the crates the manifest lays out: each name a row,
/// none twice, and something class A.
pub(super) fn hold(table: Option<&Review>, crates: &[Crate]) -> Result<()> {
    let Some(table) = table else {
        return Ok(());
    };
    for (n, name) in table.crates.iter().enumerate() {
        if !crates.iter().any(|k| &k.name == name) {
            bail!("[review] crates names {name:?}, which is no [[crate]] row");
        }
        if table.crates[..n].contains(name) {
            bail!("[review] crates names {name:?} twice");
        }
    }
    if table.crates.is_empty() && !table.unsafe_lines {
        bail!(
            "[review] names no crate and leaves out unsafe lines, so no diff is class A; a table that classes nothing is left out of the file instead"
        );
    }
    Ok(())
}

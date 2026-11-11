//! The differential lane's pure part (harness §4.2, design §8; #3740,
//! #4489): the programs a run draws beside its corpus, the grammar's
//! ([`program`]) and a corpus program's mutations ([`mutated`]). Nothing
//! here runs a program or reads a file: `xtask fuzz`, the adapter under
//! `xtask/src/fuzz`, runs both sides, compares them and records the
//! readings, and calls this crate for what a run holds. It depends on no
//! workspace crate.

#![forbid(unsafe_code)]

mod generate;
#[cfg(test)]
mod tests;

pub use generate::{Rng, mutated, program};

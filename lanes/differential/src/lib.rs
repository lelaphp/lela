//! The differential lane's pure part (harness §4.2, design §8):
//! the programs a run draws beside its corpus, the grammar's
//! ([`program`]) and a corpus program's mutations ([`mutated`]), and the
//! loop that minimises a finding ([`minimised`]). Nothing here runs a
//! program or reads a file: `xtask fuzz`, the adapter under
//! `xtask/src/fuzz`, runs both sides, compares them and records the
//! readings, and calls this crate for what a run holds and how a finding
//! shrinks. It depends on no workspace crate.

#![forbid(unsafe_code)]

mod generate;
#[cfg(test)]
mod tests;

pub use generate::{Rng, mutated, program};

/// `code` with as many of its lines dropped as leave its finding
/// standing, as `stands` judges each smaller program: runs of lines
/// halving down to one, within `trials` calls of `stands` and for as long
/// as `spent` says the run's budget is not. The opening tag stays. What
/// is left is the last program `stands` held, or `code` where it held
/// none; an error from `stands` ends the loop with it.
pub fn minimised<E>(
    code: &str,
    trials: usize,
    mut stands: impl FnMut(&str) -> Result<bool, E>,
    spent: impl Fn() -> bool,
) -> Result<String, E> {
    let render = |lines: &[&str]| {
        lines.iter().fold(String::new(), |mut out, line| {
            out.push_str(line);
            out.push('\n');
            out
        })
    };
    let mut lines: Vec<&str> = code.lines().collect();
    let head = usize::from(lines.first().is_some_and(|l| l.starts_with("<?php")));
    let mut chunk = lines.len().saturating_sub(head).div_ceil(2).max(1);
    let mut tried = 0;
    loop {
        let mut at = head;
        while at < lines.len() && tried < trials && !spent() {
            let end = (at + chunk).min(lines.len());
            let fewer: Vec<&str> = lines[..at].iter().chain(&lines[end..]).copied().collect();
            tried += 1;
            if stands(&render(&fewer))? {
                lines = fewer;
            } else {
                at = end;
            }
        }
        if chunk == 1 || tried >= trials || spent() {
            return Ok(render(&lines));
        }
        chunk = chunk.div_ceil(2);
    }
}

//! A `--PHPDBG--` test, run as run-tests.php runs one at the pinned tag
//! (run-tests.php:1904-1919, 2038, 2134, 2305, 3866-3868): through the
//! debugger host, `-qIb` before the program, the `--PHPDBG--` section and
//! a newline on stdin where the test has no `--STDIN--`, its `--ARGS--`
//! after a `--`, and its `SKIPIF` and `CLEAN` run beside it through the
//! debugger too, `-rr` added so each runs straight away.
//!
//! The host is the phpdbg `[sapi]` pins for the runner's control, asserted
//! by its digest before the first program it runs, and the engine's own
//! `phpdbg` beside the binary under test, `sapi/phpdbg`'s binary
//! (ADR-0008), for the engine. The sections beside a test run under the
//! pinned phpdbg whichever binary is measured, as every section runs under
//! the oracle.

use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;

use super::file::Phpt;
use crate::oracle::Pin;

/// The flags before the program: no banner, no init file, no colours
/// (run-tests.php:1908).
pub(crate) const FLAGS: [&str; 2] = ["-qIb", "-f"];

/// The flags before a section run beside the test: the same, and run it
/// straight away and quit (run-tests.php:1912).
pub(super) const SECTION_FLAGS: [&str; 2] = ["-qIb", "-rr"];

/// The pinned phpdbg, held to its digest and the pin's closure once per run: the first program
/// it would run asserts it, and every later one reads what that found.
#[derive(Debug, Default)]
pub(super) struct Pinned {
    digest: OnceLock<Result<(), String>>,
}

impl Pinned {
    /// `path` asserted, where it is the pinned phpdbg; the engine's own is
    /// held to the tree and not to a digest.
    pub(super) fn asserted(&self, pin: &Pin, path: &Path) -> Result<()> {
        let Some(pinned) = pin.sapi.phpdbg.as_ref().filter(|dbg| dbg.path == path) else {
            return Ok(());
        };
        self.digest
            .get_or_init(|| (pin.oracle().host_asserted(pinned)).map_err(|e| format!("{e:#}")))
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

/// What the test reads on stdin: its `--STDIN--`, or else its
/// `--PHPDBG--` and a newline, the commands the prompt reads.
pub(super) fn stdin(phpt: &Phpt) -> Vec<u8> {
    if let Some(stdin) = phpt.section("STDIN") {
        return stdin.to_vec();
    }
    let mut commands = phpt.section("PHPDBG").unwrap_or_default().to_vec();
    commands.push(b'\n');
    commands
}

/// The words after the program: `--` and the test's `--ARGS--` where it
/// has them, which phpdbg hands the program as they stand.
pub(super) fn args(phpt: &Phpt) -> Vec<String> {
    let Some(args) = phpt.text("ARGS") else {
        return Vec::new();
    };
    std::iter::once(String::from("--"))
        .chain(args.split_whitespace().map(str::to_owned))
        .collect()
}

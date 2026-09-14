//! The in-process adapter of design §8's `ScriptExec` seam: the engine
//! linked into the harness, one fresh engine per snippet over the
//! `Capture` sink (design §4.3 seam 4, the row that names the harness
//! among the hosts; `CONTEXT.md`, Host). The fresh-process adapter is
//! [`Oracle::run_under`](super::Oracle::run_under) beside it, and both
//! answer one [`Observation`], so what judges a witness never learns which
//! of the two served it.
//!
//! **Under the recipe.** The pin's preamble goes ahead of the snippet, so
//! the code stands on line 2 where php's own error text puts it, and the
//! recipe's ini is laid over the engine's defaults, so `precision`,
//! `error_reporting` and the rest are the pin's. An entry the engine has
//! no name for is carried and read by nobody, which is what php does with
//! `opcache.*` when opcache is not loaded.
//!
//! **The streams.** The first stream is the sink's bytes; the second is
//! `Capture`'s `stderr`, which is the CLI's second stream whole (#216):
//! each log entry with `\n` after it, as `sapi_cli_log_message` writes,
//! and what the error stream was given, in the order the request wrote
//! them. That is the stream the fresh-process adapter reads from php's
//! fd 2, and it goes through the recipe's normalisation the same way, so
//! a `stderr` fence is held to what the engine wrote and a witness with
//! no such fence asserts the engine wrote nothing there (#248).
//!
//! **A panic is a failure, not the harness's end** (design §8): the serve
//! is caught, and a panic comes back as the trouble that makes the witness
//! bork, naming the crate of the engine rather than ending the lane.

use std::panic::{self, AssertUnwindSafe};
use std::time::Instant;

use anyhow::{Result, bail};

use php_bundle::test::Capture;

use super::{Observation, Recipe, normalise};

/// One snippet through the linked engine under `recipe`, answered as the
/// fresh-process adapter answers it: both streams normalised by the
/// recipe's rules and the exit code the request ended on.
pub(crate) fn serve(recipe: &Recipe, snippet: &str) -> Result<Observation> {
    let source = format!("{}{snippet}", recipe.preamble);
    let ini: Vec<(&str, &str)> = recipe
        .ini
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .collect();
    let started = Instant::now();
    // A fresh engine per snippet, which is the isolation a process gave
    // (design §8).
    let served = panic::catch_unwind(AssertUnwindSafe(|| {
        php_bundle::test::engine(&ini).serve_capture(&source, Capture::default())
    }));
    let wall = started.elapsed();
    let Ok(served) = served else {
        bail!("the linked engine panicked; the panic itself is on the harness's second stream");
    };
    Ok(Observation {
        stdout: normalise(&recipe.normalise, served.stdout),
        stderr: normalise(&recipe.normalise, served.stderr),
        exit: Some(served.exit_code),
        wall,
    })
}

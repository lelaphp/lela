//! The in-process adapter of design §8's `ScriptExec` seam: the engine
//! linked into the harness, one fresh engine per snippet over the
//! `Capture` sink (design §4.3 seam 4, the row that names the harness
//! among the hosts; `CONTEXT.md`, Host). The fresh-process adapter is
//! [`Oracle::run_under`](super::Oracle::run_under) beside it, and both
//! answer one [`Observation`], so what judges a witness never learns which
//! of the two served it.
//!
//! **Under the recipe.** The pin's preamble goes ahead of the snippet, so
//! the code stands on line 2 where php's own error text puts it; the
//! request is named the recipe's `script_name`, so `__FILE__` and the
//! `in %s on line %d` of a diagnostic read `witness.php` as they do under
//! the fresh-process adapter, and not `Standard input code`, the name
//! `serve_capture` gives a piped source (#293); the file stands in a
//! scratch tree of this run's own and the request's working directory is
//! that tree, as the fresh-process adapter runs the program in its
//! directory, so an `include` of the witness's own file reads the same
//! file under both adapters and the tree's path is normalised to the
//! recipe's token where php's would be (#396); and the recipe's ini is
//! laid over the engine's defaults, so `precision`, `error_reporting` and
//! the rest are the pin's, and a named `[regime.<name>]` of the pin lays
//! its own after them, in the order the fresh-process adapter passes the
//! two as `-d` (harness-design.md §8, ledger 55). An entry the engine has
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

use php_bundle::test::{Capture, Entered};

use super::{Observation, Program, Recipe, Regime, Scratch};

/// One snippet through the linked engine under `recipe`, and under
/// `regime`'s ini after it where a run names one, answered as the
/// fresh-process adapter answers it: the program written where that
/// adapter writes it and served in that directory, both streams
/// normalised by the recipe's rules with the directory as the token, and
/// the exit code the request ended on.
pub(crate) fn serve(
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
) -> Result<Observation> {
    run(recipe, regime, snippet, false).map(|(observed, _)| observed)
}

/// [`serve`] with the natives the request entered recorded beside what
/// it observed, in the order it entered them: what witness coverage
/// counts (harness §4.5 condition 2, revision 58).
pub(crate) fn serve_traced(
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
) -> Result<(Observation, Vec<Entered>)> {
    run(recipe, regime, snippet, true)
}

fn run(
    recipe: &Recipe,
    regime: Option<&Regime>,
    snippet: &str,
    traced: bool,
) -> Result<(Observation, Vec<Entered>)> {
    let source = format!("{}{snippet}", recipe.preamble);
    let scratch = Scratch::for_program(recipe, Program::Snippet(snippet))?;
    // The recipe's entries first and the regime's after them, so an entry
    // both name is the regime's, as the later `-d` is under the
    // fresh-process adapter.
    let ini: Vec<(&str, &str)> = recipe
        .ini
        .iter()
        .chain(regime.into_iter().flat_map(|r| &r.ini))
        .filter_map(|entry| entry.split_once('='))
        .collect();
    let started = Instant::now();
    // A fresh engine per snippet, which is the isolation a process gave
    // (design §8).
    let served = panic::catch_unwind(AssertUnwindSafe(|| {
        let engine = php_bundle::test::engine(&ini);
        let name = recipe.script_name.as_bytes();
        if traced {
            engine.serve_capture_traced_in(&scratch.dir, name, &source, Capture::default())
        } else {
            engine.serve_capture_in(&scratch.dir, name, &source, Capture::default())
        }
    }));
    let wall = started.elapsed();
    let Ok(served) = served else {
        bail!("the linked engine panicked; the panic itself is on the harness's second stream");
    };
    let observed = Observation {
        stdout: scratch.normalise(&recipe.normalise, served.stdout),
        stderr: scratch.normalise(&recipe.normalise, served.stderr),
        exit: Some(served.exit_code),
        wall,
    };
    Ok((observed, served.entered))
}

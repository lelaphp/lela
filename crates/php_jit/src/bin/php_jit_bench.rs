//! The program of `xtask bench`'s `jit-wrapper` bars (#2742, #2879):
//! `php_jit_bench <wrapper|wrapper-bare|direct> <calls>` makes that many calls of the
//! test kernel one way and prints the tally they leave, which is the same
//! whichever way they went, so the two arms' bytes compare (measurement
//! protocol §5).
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use std::process::ExitCode;

use php_jit::bench::{Through, calls};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(through), Some(n)) = (
        args.first().and_then(|name| Through::named(name)),
        args.get(1).and_then(|n| n.parse::<u64>().ok()),
    ) else {
        eprintln!("usage: php_jit_bench <wrapper|wrapper-bare|direct> <calls>");
        return ExitCode::FAILURE;
    };
    match calls(through, n) {
        Ok(tally) => {
            println!("{tally}");
            ExitCode::SUCCESS
        }
        Err(fatal) => {
            eprintln!("php_jit_bench: {fatal:?}");
            ExitCode::FAILURE
        }
    }
}

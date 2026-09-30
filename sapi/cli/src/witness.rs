//! `php-witness`, the witness lane's `php`: the CLI host unchanged, built
//! with `php_engine`'s `witness-trace` feature, so that a fence the lane
//! serves in a process of its own (a fork, a signal, the process's ids or
//! limits) is covered by the natives it entered, as a fence served by the
//! engine linked into the harness is (harness §4.5 condition 2, §8,
//! revision 61). A name of its own keeps `target/*/php` the default build.
#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    php_sapi_cli::main()
}

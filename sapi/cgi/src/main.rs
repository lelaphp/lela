//! `lela-cgi`, the binary (design §4.1; ADR-0008, ADR-0016): php's CGI
//! host, [`php_sapi_cgi::main`], built beside `lela` as php builds its
//! CGI, where a test that looks for it looks
//! (`Z/sapi/cgi/tests/include.inc`).
#![deny(unsafe_code)]
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

fn main() -> ExitCode {
    php_sapi_cgi::main()
}
